//! The Meet tab's video for the call that records. mc-meet records the tab in Chrome and sends
//! fMP4 fragments over native messaging; this task writes them into the call folder with
//! `video_capture::TabVideoWriter`, beside the audio process's stems. No capture process runs:
//! the app only writes files, so it needs no Screen Recording grant.
//!
//! While the task runs, mc-meet hears through `call_status` that the call wants its tab video.
//! A stop tells it first and then waits up to 2 s for the last fragment its `stopped` names. A video segment
//! pairs with one audio segment and follows it across a sleep: the audio's sleep report closes
//! the segment, and the next fragment after the wake opens one on the audio's new number.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use tauri::{AppHandle, Manager as _};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::Instrument as _;
use video_capture::{AudioPairing, TabFragment, TabFragmentOutcome, TabSegmentEnd, TabVideoWriter};

use super::{AudioPhase, RunningProcess, VideoState};
use crate::AppState;
use crate::browser_calls::{CaptureEnd, TabVideoInput, TabVideoState};
use crate::call_status::{Moment, Observation, display_time_zone};
use crate::calls::Call;

/// Fragments that may wait for the writer: about 96 s of video. Above the 30 fragments mc-meet
/// holds while its port is down, so the burst it sends after a reconnect fits. A fuller queue
/// means the disk stalls, and the relay drops the newest.
const SINK_DEPTH: usize = 48;
/// mc-meet sends the last fragment of a stopped capture within this long.
const STOP_FLUSH_LIMIT: Duration = Duration::from_secs(2);

/// Starts the call's tab video task. Dropping or sending `stop` ends it.
pub(super) fn spawn(
    app: &AppHandle,
    call: &Call,
    folder: &Path,
    audio_phase: watch::Receiver<AudioPhase>,
) -> RunningProcess {
    let (stop, stop_receiver) = oneshot::channel();
    let span = tracing::info_span!(
        "capture.tab_video",
        call.id = %call.id.display(),
    );
    let task = TabVideoTask {
        app: app.clone(),
        call_id: call.id.clone(),
        folder: folder.to_path_buf(),
        writer: Some(TabVideoWriter::new(folder, &call.id.display().to_string())),
        is_recorded: false,
        opened_at: Timestamp::now(),
        stopped_at: None,
        last_arrived: None,
        logged_capture_id: None,
    };
    let task = tauri::async_runtime::spawn(task.run(stop_receiver, audio_phase).instrument(span));
    RunningProcess { stop, task }
}

struct TabVideoTask {
    app: AppHandle,
    call_id: PathBuf,
    folder: PathBuf,
    /// `None` only while a blocking write holds it.
    writer: Option<TabVideoWriter>,
    /// A fragment of this call reached the disk in this run.
    is_recorded: bool,
    /// Frames from before this are dropped. mc-meet may still hold fragments recorded before a
    /// stop, and this task records only after its own start.
    opened_at: Timestamp,
    /// When the user's stop reached the task. Frames from after it are not recorded.
    stopped_at: Option<Timestamp>,
    /// The capture and index of the last fragment that arrived.
    last_arrived: Option<(String, u64)>,
    /// The capture whose playback latency the log has.
    logged_capture_id: Option<String>,
}

impl TabVideoTask {
    async fn run(
        mut self,
        mut stop: oneshot::Receiver<()>,
        mut audio_phase: watch::Receiver<AudioPhase>,
    ) {
        let (sink, mut inputs) = mpsc::channel(SINK_DEPTH);
        self.state()
            .browser_calls
            .open_tab_video(&self.call_id, sink);
        self.publish_call_status();
        self.show(VideoState::TabWaiting);
        let mut seen_sleep_count = audio_phase.borrow_and_update().sleep_count;
        let mut is_audio_gone = false;
        loop {
            tokio::select! {
                _ = &mut stop => break,
                input = inputs.recv() => match input {
                    Some(input) => {
                        let phase = *audio_phase.borrow();
                        self.take(input, phase).await;
                    }
                    None => break,
                },
                changed = audio_phase.changed(), if !is_audio_gone => {
                    if changed.is_err() {
                        is_audio_gone = true;
                        continue;
                    }
                    let phase = *audio_phase.borrow_and_update();
                    // A sleep and its wake may both come between two reads.
                    if phase.is_asleep || phase.sleep_count != seen_sleep_count {
                        self.close_segment(TabSegmentEnd::Sleep).await;
                    }
                    seen_sleep_count = phase.sleep_count;
                    if phase.is_asleep {
                        self.show(VideoState::Sleeping);
                    }
                }
            }
        }
        self.stopped_at = Some(Timestamp::now());
        self.state()
            .browser_calls
            .set_tab_video_wanted(&self.call_id, false);
        self.publish_call_status();
        self.drain_stopped_capture(&mut inputs, &audio_phase).await;
        self.state().browser_calls.close_tab_video(&self.call_id);
        self.publish_call_status();
        self.close_segment(TabSegmentEnd::Stopped).await;
    }

    /// Takes what mc-meet sends after the stop until the capture's last fragment arrived, the
    /// capture failed, or the limit passed. A `stopped` may arrive before its last fragment.
    async fn drain_stopped_capture(
        &mut self,
        inputs: &mut mpsc::Receiver<TabVideoInput>,
        audio_phase: &watch::Receiver<AudioPhase>,
    ) {
        let flush_deadline = tokio::time::Instant::now() + STOP_FLUSH_LIMIT;
        let mut awaited_end: Option<CaptureEnd> = None;
        while let Ok(Some(input)) = tokio::time::timeout_at(flush_deadline, inputs.recv()).await {
            let phase = *audio_phase.borrow();
            let mut is_capture_over = false;
            match &input {
                TabVideoInput::Fragment(_) => {}
                TabVideoInput::State { state, ended, .. } => match state {
                    TabVideoState::Stopped => match ended {
                        Some(end) => awaited_end = Some(end.clone()),
                        // An older mc-meet names no last fragment; its stop flushed already.
                        None => is_capture_over = true,
                    },
                    TabVideoState::Failed => is_capture_over = true,
                    TabVideoState::NeedsClick | TabVideoState::Capturing => {}
                },
            }
            self.take(input, phase).await;
            if is_capture_over
                || awaited_end
                    .as_ref()
                    .is_some_and(|end| self.has_arrived(end))
            {
                return;
            }
        }
        if let Some(end) = awaited_end {
            tracing::warn!(
                call.id = %self.call_id.display(),
                video.capture_id = end.capture_id,
                video.last_fragment_index = end.last_index,
                "video.tab_last_fragment_missing"
            );
        }
    }

    /// The capture's last fragment arrived, written or cut at the stop time.
    fn has_arrived(&self, end: &CaptureEnd) -> bool {
        end.last_index == 0
            || self
                .last_arrived
                .as_ref()
                .is_some_and(|(capture_id, index)| {
                    *capture_id == end.capture_id && *index >= end.last_index
                })
    }

    async fn take(&mut self, input: TabVideoInput, phase: AudioPhase) {
        match input {
            TabVideoInput::Fragment(fragment) => {
                self.last_arrived = Some((fragment.capture_id.clone(), fragment.index));
                // Fragments are cut whole, so the one holding the stop goes too: no video after
                // the stop is kept, at the cost of up to 2 s before it.
                if let Some(stopped_at) = self.stopped_at
                    && fragment.end() > stopped_at
                {
                    tracing::debug!(
                        call.id = %self.call_id.display(),
                        video.capture_id = fragment.capture_id,
                        video.fragment_index = fragment.index,
                        "video.fragment_after_stop"
                    );
                    return;
                }
                if fragment.first_frame_at < self.opened_at {
                    tracing::debug!(
                        call.id = %self.call_id.display(),
                        video.capture_id = fragment.capture_id,
                        video.fragment_index = fragment.index,
                        "video.fragment_before_open"
                    );
                    return;
                }
                // Frames from before the sleep: their segment is closed and no audio pairs.
                if !phase.is_asleep {
                    self.log_new_capture(&fragment);
                    let audio = AudioPairing {
                        segment: phase.segment,
                        sleep_count: phase.sleep_count,
                        awake_since: phase.awake_since,
                    };
                    self.write(fragment, audio).await;
                }
            }
            TabVideoInput::State { state, detail, .. } => match state {
                TabVideoState::NeedsClick => self.show(VideoState::TabNeedsClick),
                TabVideoState::Capturing => {}
                TabVideoState::Stopped => self.show(VideoState::TabStopped),
                TabVideoState::Failed => {
                    tracing::warn!(
                        call.id = %self.call_id.display(),
                        exception.message = detail,
                        "video.tab_capture_failed"
                    );
                    self.show(VideoState::TabFailed);
                }
            },
        }
    }

    async fn write(&mut self, fragment: TabFragment, audio: AudioPairing) {
        let Some(mut writer) = self.writer.take() else {
            return;
        };
        let joined = tauri::async_runtime::spawn_blocking(move || {
            let outcome = writer.write(&fragment, audio);
            (writer, fragment, outcome)
        })
        .await;
        let (writer, fragment, outcome) = match joined {
            Ok(joined) => joined,
            Err(error) => {
                tracing::error!(exception.message = %error, "video.tab_write_task_failed");
                self.writer = Some(self.fresh_writer());
                return;
            }
        };
        self.writer = Some(writer);
        match outcome {
            Ok(TabFragmentOutcome::Written {
                segment,
                is_segment_new,
                fragment: written,
            }) => {
                tracing::debug!(
                    segment.number = segment,
                    video.capture_id = fragment.capture_id,
                    video.fragment_index = fragment.index,
                    file.name = written.file,
                    file.size = written.bytes,
                    "video.tab_fragment_written"
                );
                if is_segment_new || !self.is_recorded {
                    self.show(VideoState::Recording { segment });
                }
                if !self.is_recorded {
                    self.is_recorded = true;
                    self.state().calls.status().observe(
                        &self.call_id,
                        Observation::TabVideoRecorded,
                        Moment::now(),
                    );
                }
            }
            Ok(TabFragmentOutcome::Repeat) => {}
            Ok(TabFragmentOutcome::BeforeSegment) => tracing::info!(
                call.id = %self.call_id.display(),
                video.capture_id = fragment.capture_id,
                video.fragment_index = fragment.index,
                "video.fragment_before_segment"
            ),
            Ok(TabFragmentOutcome::StaleClock { step_ms }) => tracing::info!(
                call.id = %self.call_id.display(),
                video.capture_id = fragment.capture_id,
                video.fragment_index = fragment.index,
                video.clock_step_ms = step_ms,
                "video.fragment_stale_clock"
            ),
            Ok(TabFragmentOutcome::FromEndedCapture) => tracing::debug!(
                call.id = %self.call_id.display(),
                video.capture_id = fragment.capture_id,
                video.fragment_index = fragment.index,
                "video.fragment_from_ended_capture"
            ),
            Err(error) => {
                tracing::error!(
                    call.id = %self.call_id.display(),
                    video.capture_id = fragment.capture_id,
                    video.fragment_index = fragment.index,
                    exception.message = %error,
                    "video.tab_fragment_failed"
                );
                self.show(VideoState::Failing);
            }
        }
    }

    /// Logs each capture's playback latencies once. Finalize uses the output latency; the base
    /// latency tells how much of it the output device adds, a Bluetooth one most.
    fn log_new_capture(&mut self, fragment: &TabFragment) {
        if self.logged_capture_id.as_deref() == Some(fragment.capture_id.as_str()) {
            return;
        }
        self.logged_capture_id = Some(fragment.capture_id.clone());
        tracing::info!(
            call.id = %self.call_id.display(),
            video.capture_id = fragment.capture_id,
            video.codec = fragment.codec.as_str(),
            video.playback_latency_ms = fragment.playback_latency_ms,
            video.playback_base_latency_ms = fragment.playback_base_latency_ms,
            "video.tab_capture_started"
        );
    }

    async fn close_segment(&mut self, end: TabSegmentEnd) {
        let Some(mut writer) = self.writer.take() else {
            return;
        };
        let joined = tauri::async_runtime::spawn_blocking(move || {
            let closed = writer.close_segment(end);
            (writer, closed)
        })
        .await;
        match joined {
            Ok((writer, closed)) => {
                self.writer = Some(writer);
                if let Err(error) = closed {
                    tracing::error!(
                        call.id = %self.call_id.display(),
                        exception.message = %error,
                        "video.tab_segment_close_failed"
                    );
                }
            }
            Err(error) => {
                tracing::error!(exception.message = %error, "video.tab_write_task_failed");
                self.writer = Some(self.fresh_writer());
            }
        }
    }

    /// A writer with no segment open, after a panic took the last one down. Its segment stays
    /// open on disk until the next repair.
    fn fresh_writer(&self) -> TabVideoWriter {
        TabVideoWriter::new(&self.folder, &self.call_id.display().to_string())
    }

    fn state(&self) -> &AppState {
        self.app.state::<AppState>().inner()
    }

    fn show(&self, state: VideoState) {
        self.state()
            .call_capture
            .set_video_state(&self.app, &self.call_id, Some(state));
    }

    /// At once, so mc-meet starts or stops the capture now, not at the next status tick.
    fn publish_call_status(&self) {
        let state = self.state();
        state.browser_calls.publish_call_status(
            &state.calls.status().view(Moment::now()),
            &display_time_zone(state),
        );
    }
}
