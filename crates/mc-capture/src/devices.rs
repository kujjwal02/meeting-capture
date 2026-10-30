//! When a segment moves a stem to another device. The HAL listeners only flag changes; this
//! module decides, from those flags and what the stems delivered, whether to swap the mic or
//! rebuild the tap. It reads the HAL only after a change, never on a timer.
//!
//! - Changes settle first: 1 s, or 1.5 s when Bluetooth joined or left the route. Rebuilds
//!   during Bluetooth churn fail.
//! - The mic moves when the rule picks another input, or when its device went away. The new
//!   stream opens beside the old one. It takes over on its first non-zero buffer, or after 2 s
//!   of zeros counted from its first buffer. A stream with no buffer within 5 s is dropped.
//! - A mid-call pick under `AvoidBluetooth` never returns a Bluetooth, virtual or
//!   unknown-transport input. With no safe input left, a changed pick keeps the old stream and
//!   a lost mic leaves the stem with no mic until one appears.
//! - The tap is rebuilt only on evidence: its aggregate died, its format changed rate, its
//!   buffers arrive at another rate than their stamps say, it went stale after an output route
//!   change, or it delivered nothing for 5 s. Silence is never evidence. At most 4 rebuilds per
//!   episode, 15 s apart; an episode ends after 60 s without one.

use std::time::{Duration, Instant};

use capture::{
    BluetoothRoute, BufferStamp, CaptureError, HalChanges, MicMoment, MicRequest, MicRule,
    MicStream, StemReader, TapHealth,
};
use capture_protocol::DeviceChangeCause;

/// Quiet time after a device change before acting on it.
const SETTLE: Duration = Duration::from_secs(1);
/// Quiet time after a change that moved Bluetooth in or out of the route.
const BLUETOOTH_SETTLE: Duration = Duration::from_millis(1_500);
/// A tap with no buffer for this long after a route change is stale. It delivers every 10 ms
/// or so when healthy, silence included.
const STALE_LIMIT: Duration = Duration::from_secs(1);
/// How long after a default-output change a stale tap still counts as caused by it.
const ROUTE_WATCH: Duration = Duration::from_secs(10);
/// A tap with no buffer for this long is stalled, route change or not.
const STALL_LIMIT: Duration = Duration::from_secs(5);
const REBUILD_COOLDOWN: Duration = Duration::from_secs(15);
const REBUILDS_PER_EPISODE: u32 = 4;
/// An episode ends once this long passes without a rebuild.
const EPISODE_QUIET: Duration = Duration::from_secs(60);
/// A mic that would not open is tried again after this long, if nothing changes first.
const MIC_RETRY: Duration = Duration::from_secs(15);
/// A new mic stream that delivers only digital zeros this long after its first buffer takes
/// over anyway: the rule chose it, and the old one may be gone.
const MIC_SWAP_SILENCE_LIMIT: Duration = Duration::from_secs(2);
/// A new mic stream with no buffer by this long after its open is dropped.
const MIC_SWAP_FIRST_BUFFER_LIMIT: Duration = Duration::from_secs(5);
/// The delivered-rate check measures over windows of at least this much host time.
const RATE_WINDOW_NS: u64 = 2_000_000_000;
/// A window whose median buffer pace is this far from the stamps' rate is a mismatch.
const RATE_TOLERANCE: f64 = 0.02;
/// Mismatched windows in a row that count as evidence. One window can hold a route glitch.
const RATE_MISMATCH_WINDOWS: u32 = 2;

/// What the rebuild limits allow now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebuildDecision {
    Allowed,
    CoolingDown,
    Exhausted,
}

/// What a new mic stream is ready for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateState {
    Waiting,
    Ready,
    /// No buffer within 5 s of its open.
    NoBuffers,
}

/// What to do with the mic after a device change.
pub(crate) enum MicSwap {
    /// Open a new stream for this cause, with this request.
    Open(DeviceChangeCause, MicRequest),
    /// The mic is lost and no safe input is left: record no mic until one appears.
    Unavailable(DeviceChangeCause, CaptureError),
}

/// The device rules' state for one segment.
pub(crate) struct DeviceWatch {
    rule: MicRule,
    route: BluetoothRoute,
    settle_until: Option<Instant>,
    is_mic_check_due: bool,
    mic_retry_at: Option<Instant>,
    output_changed_at: Option<Instant>,
    /// The built-in mic's UID while the lid is closed and the mic delivered only zeros.
    lid_muted_uid: Option<String>,
    /// Evidence that already led to a rebuild whose open failed, so the tap stays closed until
    /// the next try.
    unfinished_rebuild: Option<DeviceChangeCause>,
    rebuild_count: u32,
    last_rebuild_at: Option<Instant>,
}

impl DeviceWatch {
    /// `lid_muted_uid` carries the skipped built-in mic over from the call's last segment.
    pub(crate) fn new(rule: MicRule, lid_muted_uid: Option<String>) -> Self {
        Self {
            rule,
            route: capture::bluetooth_route(),
            settle_until: None,
            is_mic_check_due: false,
            mic_retry_at: None,
            output_changed_at: None,
            lid_muted_uid,
            unfinished_rebuild: None,
            rebuild_count: 0,
            last_rebuild_at: None,
        }
    }

    /// Takes the listeners' flags and restarts the settle time when any are set.
    pub(crate) fn take_changes(&mut self, now: Instant) -> HalChanges {
        let changes = capture::take_hal_changes();
        if !changes.any() {
            return changes;
        }
        let route = capture::bluetooth_route();
        let is_bluetooth_change = route != self.route;
        self.route = route;
        let settle = if is_bluetooth_change {
            BLUETOOTH_SETTLE
        } else {
            SETTLE
        };
        // A short settle after a Bluetooth one must not end the Bluetooth wait early.
        let until = now + settle;
        self.settle_until = Some(
            self.settle_until
                .map_or(until, |existing| existing.max(until)),
        );
        if changes.default_input || changes.device_list {
            self.is_mic_check_due = true;
            self.mic_retry_at = None;
        }
        // A device-list change alone is no route change: the tap's own aggregate fires one
        // whenever it opens or closes.
        if changes.default_output {
            self.output_changed_at = Some(now);
        }
        tracing::info!(
            hal.default_input_changed = changes.default_input,
            hal.default_output_changed = changes.default_output,
            hal.device_list_changed = changes.device_list,
            bluetooth.default_input = route.default_input_is_bluetooth,
            bluetooth.default_output = route.default_output_is_bluetooth,
            bluetooth.devices = route.bluetooth_devices,
            device.settle_ms = u64::try_from(settle.as_millis()).unwrap_or(0),
            "device.changes"
        );
        changes
    }

    fn is_settled(&self, now: Instant) -> bool {
        self.settle_until.is_none_or(|until| now >= until)
    }

    /// The built-in mic went quiet with the lid closed: pick without it until the lid opens.
    pub(crate) fn skip_lid_muted_mic(&mut self, uid: Option<String>) {
        self.lid_muted_uid = uid;
    }

    /// The built-in mic skipped since the lid closed, for the call's next segment.
    pub(crate) fn lid_muted_uid(&self) -> Option<String> {
        self.lid_muted_uid.clone()
    }

    /// The pick request at `moment`. Reads the lid state only while the built-in mic is
    /// skipped.
    pub(crate) fn mic_request(&mut self, moment: MicMoment) -> MicRequest {
        if self.lid_muted_uid.is_some() && is_lid_closed() == Some(false) {
            tracing::info!("mic.lid_opened");
            self.lid_muted_uid = None;
        }
        MicRequest {
            rule: self.rule,
            moment,
            skipped_uid: self.lid_muted_uid.clone(),
        }
    }

    /// What to do with the mic now, if anything. `lost_cause` is set while the mic is lost or
    /// absent, and `mic` is `None` while absent. Reads the HAL only when a change or a retry
    /// is due.
    pub(crate) fn mic_swap(
        &mut self,
        now: Instant,
        mic: Option<&MicStream>,
        lost_cause: Option<DeviceChangeCause>,
    ) -> Option<MicSwap> {
        if !self.is_settled(now) || self.mic_retry_at.is_some_and(|retry_at| now < retry_at) {
            return None;
        }
        let (Some(mic), None) = (mic, lost_cause) else {
            let cause = lost_cause.unwrap_or(DeviceChangeCause::MicLost);
            self.is_mic_check_due = false;
            let request = self.mic_request(MicMoment::MidCall);
            return Some(match capture::mic_pick(&request) {
                Ok(_) => MicSwap::Open(cause, request),
                Err(error) => {
                    self.mic_open_failed(now);
                    MicSwap::Unavailable(cause, error)
                }
            });
        };
        if !std::mem::take(&mut self.is_mic_check_due) {
            return None;
        }
        let request = self.mic_request(MicMoment::MidCall);
        let pick = match capture::mic_pick(&request) {
            Ok(pick) => pick,
            Err(error) => {
                // No safe input: the old stream records on.
                tracing::warn!(exception.message = %error, "mic.pick_failed");
                return None;
            }
        };
        let current = mic.pick();
        tracing::info!(
            device.name = pick.device_name.as_deref(),
            mic.transport = %pick.transport,
            mic.rule = %pick.rule,
            mic.reason = %pick.reason,
            mic.default_input = pick.default_input_name.as_deref(),
            mic.default_input_transport = %pick.default_input_transport,
            mic.current_device = current.device_name.as_deref(),
            "mic.repicked"
        );
        (pick.device_uid != current.device_uid)
            .then_some(MicSwap::Open(DeviceChangeCause::MicPickChanged, request))
    }

    /// A new mic stream did not open. Try again after `MIC_RETRY` or at the next change.
    pub(crate) fn mic_open_failed(&mut self, now: Instant) {
        self.mic_retry_at = Some(now + MIC_RETRY);
    }

    /// Why to rebuild the tap now, if at all. `health` is `None` while the tap is closed after a
    /// failed rebuild. `last_delivery` is the later of the tap's last buffer and its open.
    pub(crate) fn tap_rebuild_cause(
        &mut self,
        now: Instant,
        health: Option<TapHealth>,
        last_delivery: Option<Instant>,
        is_rate_mismatched: bool,
    ) -> Option<DeviceChangeCause> {
        if self
            .output_changed_at
            .is_some_and(|changed_at| now.duration_since(changed_at) > ROUTE_WATCH)
        {
            self.output_changed_at = None;
        }
        if !self.is_settled(now) {
            return None;
        }
        let Some(health) = health else {
            return self.unfinished_rebuild;
        };
        if health.is_aggregate_dead {
            return Some(DeviceChangeCause::TapAggregateDied);
        }
        if health.format_changes > 0 {
            return Some(DeviceChangeCause::TapFormatChanged);
        }
        if is_rate_mismatched {
            return Some(DeviceChangeCause::TapRateMismatch);
        }
        let quiet = last_delivery.map(|at| now.duration_since(at));
        if quiet.is_some_and(|quiet| quiet >= STALL_LIMIT) {
            return Some(DeviceChangeCause::TapStalled);
        }
        let is_stale = quiet.is_none_or(|quiet| quiet >= STALE_LIMIT);
        (self.output_changed_at.is_some() && is_stale)
            .then_some(DeviceChangeCause::TapStaleAfterRouteChange)
    }

    /// Counts a rebuild when the limits allow one.
    pub(crate) fn take_rebuild(&mut self, now: Instant) -> RebuildDecision {
        if let Some(last) = self.last_rebuild_at {
            let since = now.duration_since(last);
            if since < REBUILD_COOLDOWN {
                return RebuildDecision::CoolingDown;
            }
            if since >= EPISODE_QUIET {
                self.rebuild_count = 0;
            }
        }
        if self.rebuild_count >= REBUILDS_PER_EPISODE {
            return RebuildDecision::Exhausted;
        }
        self.rebuild_count += 1;
        self.last_rebuild_at = Some(now);
        RebuildDecision::Allowed
    }

    pub(crate) fn rebuild_count(&self) -> u32 {
        self.rebuild_count
    }

    /// The tap reopened: its evidence starts over.
    pub(crate) fn tap_rebuilt(&mut self) {
        self.output_changed_at = None;
        self.unfinished_rebuild = None;
    }

    /// The rebuild's open failed: the tap stays closed and the next try waits for the cooldown.
    pub(crate) fn tap_open_failed(&mut self, cause: DeviceChangeCause) {
        self.unfinished_rebuild = Some(cause);
        self.output_changed_at = None;
    }
}

/// Measures the pace the tap delivers buffers at, against the rate its stamps carry. A tap
/// whose format listener missed a change stamps the old rate on buffers of the new one.
#[derive(Default)]
pub(crate) struct DeliveredRate {
    previous: Option<BufferStamp>,
    window_start_ns: Option<u64>,
    /// Host time per buffer over the time its frames take at the stamped rate.
    paces: Vec<f64>,
    mismatched_windows: u32,
    /// Log the next finished window at info level: the first after an open or a route change.
    is_next_window_logged: bool,
}

impl DeliveredRate {
    pub(crate) fn new() -> Self {
        Self {
            is_next_window_logged: true,
            ..Self::default()
        }
    }

    pub(crate) fn log_next_window(&mut self) {
        self.is_next_window_logged = true;
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "host-time steps and frame counts are far below 2^52"
    )]
    pub(crate) fn observe(&mut self, stamps: &[BufferStamp]) {
        for &stamp in stamps {
            let Some(previous) = self.previous.replace(stamp) else {
                self.window_start_ns = Some(stamp.host_time_ns);
                continue;
            };
            if previous.frame_count == 0 || previous.sample_rate_hz == 0 {
                continue;
            }
            let step_ns = stamp.host_time_ns.saturating_sub(previous.host_time_ns) as f64;
            let expected_ns =
                f64::from(previous.frame_count) * 1e9 / f64::from(previous.sample_rate_hz);
            self.paces.push(step_ns / expected_ns);
            let window_start_ns = *self.window_start_ns.get_or_insert(previous.host_time_ns);
            if stamp.host_time_ns.saturating_sub(window_start_ns) >= RATE_WINDOW_NS {
                self.finish_window(stamp.sample_rate_hz);
                self.window_start_ns = Some(stamp.host_time_ns);
            }
        }
    }

    fn finish_window(&mut self, stamped_rate_hz: u32) {
        let mut paces = std::mem::take(&mut self.paces);
        paces.sort_unstable_by(f64::total_cmp);
        let Some(&median) = paces.get(paces.len() / 2) else {
            return;
        };
        let delivered_hz = f64::from(stamped_rate_hz) / median;
        let is_mismatched = (median - 1.0).abs() > RATE_TOLERANCE;
        if is_mismatched {
            self.mismatched_windows += 1;
            tracing::warn!(
                audio.sample_rate_hz = stamped_rate_hz,
                audio.delivered_rate_hz = delivered_hz,
                tap.mismatched_windows = self.mismatched_windows,
                "tap.rate_mismatch"
            );
        } else {
            self.mismatched_windows = 0;
            if std::mem::take(&mut self.is_next_window_logged) {
                tracing::info!(
                    audio.sample_rate_hz = stamped_rate_hz,
                    audio.delivered_rate_hz = delivered_hz,
                    "tap.delivered_rate"
                );
            }
        }
        self.paces = paces;
        self.paces.clear();
    }

    pub(crate) fn is_mismatched(&self) -> bool {
        self.mismatched_windows >= RATE_MISMATCH_WINDOWS
    }
}

/// A mic stream opened beside the recording one. It records nothing until it takes over.
pub(crate) struct MicCandidate {
    pub stream: MicStream,
    pub reader: StemReader,
    pub cause: DeviceChangeCause,
    stamps: Vec<BufferStamp>,
    samples: Vec<f32>,
    /// Stream index of the first sample that is not exactly zero.
    first_sound_index: Option<u64>,
    opened_at: Instant,
    first_buffer_at: Option<Instant>,
    /// Debug builds' injected event: the stream's buffers are thrown away.
    is_silenced: bool,
}

/// Whether the lid is closed. Debug builds can inject the state.
pub(crate) fn is_lid_closed() -> Option<bool> {
    #[cfg(debug_assertions)]
    if let Some(is_closed) = crate::inject::lid_closed() {
        return Some(is_closed);
    }
    capture::is_lid_closed()
}

/// What a candidate hands the stem at takeover: its buffers from the switch point on,
/// renumbered from 0, and how many of its samples were dropped before that point.
pub(crate) struct Takeover {
    pub stream: MicStream,
    pub reader: StemReader,
    pub stamps: Vec<BufferStamp>,
    pub samples: Vec<f32>,
    pub dropped_samples: u64,
}

impl MicCandidate {
    pub(crate) fn new(stream: MicStream, reader: StemReader, cause: DeviceChangeCause) -> Self {
        Self {
            stream,
            reader,
            cause,
            stamps: Vec::new(),
            samples: Vec::new(),
            first_sound_index: None,
            opened_at: Instant::now(),
            first_buffer_at: None,
            is_silenced: cfg!(debug_assertions) && is_candidate_silenced(),
        }
    }

    pub(crate) fn collect(&mut self) {
        if self.is_silenced {
            self.reader.drain(|_| {}, |_| {});
            return;
        }
        let start = self.samples.len();
        let (stamps, samples) = (&mut self.stamps, &mut self.samples);
        self.reader.drain(
            |stamp| stamps.push(stamp),
            |chunk| samples.extend_from_slice(chunk),
        );
        if self.first_buffer_at.is_none() && !self.stamps.is_empty() {
            self.first_buffer_at = Some(Instant::now());
        }
        if self.first_sound_index.is_none()
            && let Some(offset) = self.samples[start..].iter().position(|&s| s != 0.0)
        {
            self.first_sound_index = Some((start + offset) as u64);
        }
    }

    /// Ready on its first non-zero buffer, or after 2 s of zeros from its first buffer. A lost
    /// mic's stream delivers nothing, so the new one takes over on its first buffer.
    pub(crate) fn state(&self, now: Instant) -> CandidateState {
        let Some(first_buffer_at) = self.first_buffer_at else {
            return if now.duration_since(self.opened_at) >= MIC_SWAP_FIRST_BUFFER_LIMIT {
                CandidateState::NoBuffers
            } else {
                CandidateState::Waiting
            };
        };
        let is_ready = self.first_sound_index.is_some()
            || self.cause != DeviceChangeCause::MicPickChanged
            || now.duration_since(first_buffer_at) >= MIC_SWAP_SILENCE_LIMIT;
        if is_ready {
            CandidateState::Ready
        } else {
            CandidateState::Waiting
        }
    }

    pub(crate) fn has_sound(&self) -> bool {
        self.first_sound_index.is_some()
    }

    /// Keeps the buffers from the first one that holds sound and starts at or after
    /// `old_end_ns`, where the old stream's last buffer ended. Earlier ones are the device
    /// warming up, or time the old stream already covers.
    pub(crate) fn take_over(self, old_end_ns: Option<u64>) -> Takeover {
        let first_sound_index = self.first_sound_index.unwrap_or(0);
        let kept_from = self.stamps.iter().position(|stamp| {
            let end_index = stamp.sample_index + u64::from(stamp.accepted_count);
            end_index > first_sound_index
                && old_end_ns.is_none_or(|old_end| stamp.host_time_ns >= old_end)
        });
        let (dropped_samples, stamps) = match kept_from {
            Some(index) => {
                let dropped = self.stamps[index].sample_index;
                let stamps = self.stamps[index..]
                    .iter()
                    .map(|stamp| BufferStamp {
                        sample_index: stamp.sample_index - dropped,
                        ..*stamp
                    })
                    .collect();
                (dropped, stamps)
            }
            None => (self.samples.len() as u64, Vec::new()),
        };
        let dropped_len = usize::try_from(dropped_samples).unwrap_or(self.samples.len());
        let samples = self.samples[dropped_len.min(self.samples.len())..].to_vec();
        Takeover {
            stream: self.stream,
            reader: self.reader,
            stamps,
            samples,
            dropped_samples,
        }
    }
}

#[cfg(debug_assertions)]
fn is_candidate_silenced() -> bool {
    crate::inject::take_candidate_silenced()
}

#[cfg(not(debug_assertions))]
fn is_candidate_silenced() -> bool {
    false
}

/// When the end of a buffer falls in host time.
pub(crate) fn buffer_end_ns(stamp: &BufferStamp) -> u64 {
    if stamp.sample_rate_hz == 0 {
        return stamp.host_time_ns;
    }
    let length_ns = u64::from(stamp.frame_count) * 1_000_000_000 / u64::from(stamp.sample_rate_hz);
    stamp.host_time_ns + length_ns
}
