use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use crate::mac::tap::RemoteSource;
use crate::mic::MicSource;
use crate::{CaptureError, MicRule, Stem, StemReader};

/// The longest any one device open may take. A hung HAL call must not hang the helper.
const OPEN_LIMIT: Duration = Duration::from_secs(5);

/// How often the tap open hands control back while it waits.
const WAIT_STEP: Duration = Duration::from_millis(20);

/// Set while an open thread for the stem runs, timed out or not. A second open would race the
/// first for the same device.
static MIC_OPEN_PENDING: AtomicBool = AtomicBool::new(false);
static REMOTE_OPEN_PENDING: AtomicBool = AtomicBool::new(false);

/// The two stems of one call.
///
/// Teardown: `stop_callbacks`, then drain both readers, then drop.
pub struct CallAudioSource {
    mic: MicSource,
    #[cfg(target_os = "macos")]
    remote: Option<RemoteSource>,
}

/// A stem's reader, and how long its device took to open.
pub struct OpenedStem {
    pub reader: StemReader,
    pub open_duration: Duration,
}

pub struct CallReaders {
    pub mic: OpenedStem,
    /// `None` where no system audio source exists yet: on Windows until phase 5.
    pub remote: Option<OpenedStem>,
}

impl CallAudioSource {
    /// Opens the mic, then the system audio tap. In that order 0 of 30 starts failed; tap first,
    /// 9 of 30 did. Each open has 5 s. While the tap opens, `while_waiting` gets the mic reader
    /// about every 20 ms: the mic ring fills in about 1.4 s, so drain it there.
    ///
    /// # Errors
    ///
    /// Fails when a device does not open, an open takes longer than 5 s, or an earlier open of
    /// the same stem has not returned.
    pub fn open(
        rule: MicRule,
        mut while_waiting: impl FnMut(&mut StemReader),
    ) -> Result<(Self, CallReaders), CaptureError> {
        let opening = Instant::now();
        #[cfg_attr(
            not(target_os = "macos"),
            allow(unused_mut, reason = "only the tap open drains the mic reader")
        )]
        let (mic, mut mic_reader) = open_timed(Stem::Mic, move || MicSource::start(rule), || {})?;
        let mic_open_duration = opening.elapsed();

        #[cfg(target_os = "macos")]
        let (source, remote) = {
            let opening = Instant::now();
            let (remote, remote_reader) = open_timed(Stem::Remote, RemoteSource::start, || {
                while_waiting(&mut mic_reader);
            })?;
            let source = Self {
                mic,
                remote: Some(remote),
            };
            let remote = OpenedStem {
                reader: remote_reader,
                open_duration: opening.elapsed(),
            };
            (source, Some(remote))
        };
        #[cfg(not(target_os = "macos"))]
        let (source, remote) = {
            let _ = &mut while_waiting;
            (Self { mic }, None)
        };

        let readers = CallReaders {
            mic: OpenedStem {
                reader: mic_reader,
                open_duration: mic_open_duration,
            },
            remote,
        };
        Ok((source, readers))
    }

    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn remote(&self) -> Option<&RemoteSource> {
        self.remote.as_ref()
    }

    /// Stops every callback. The readers then hold all the audio the devices delivered; drain
    /// them before dropping this, which releases the aggregate device and the tap.
    pub fn stop_callbacks(&mut self) {
        self.mic.stop();
        #[cfg(target_os = "macos")]
        if let Some(remote) = self.remote.as_mut() {
            remote.stop_callbacks();
        }
    }
}

fn pending_flag(stem: Stem) -> &'static AtomicBool {
    match stem {
        Stem::Mic => &MIC_OPEN_PENDING,
        Stem::Remote => &REMOTE_OPEN_PENDING,
    }
}

/// Clears a stem's pending flag when the open thread ends, panics included.
struct PendingOpen(&'static AtomicBool);

impl Drop for PendingOpen {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Runs `open` on its own thread and waits at most `OPEN_LIMIT` for it.
fn open_timed<S: Send + 'static>(
    stem: Stem,
    open: impl FnOnce() -> Result<(S, StemReader), CaptureError> + Send + 'static,
    mut while_waiting: impl FnMut(),
) -> Result<(S, StemReader), CaptureError> {
    let flag = pending_flag(stem);
    if flag.swap(true, Ordering::AcqRel) {
        return Err(CaptureError::OpenPending { stem });
    }
    let pending = PendingOpen(flag);
    // A rendezvous channel: once the caller stops waiting, the send fails and the source comes
    // back to the open thread, which closes it there.
    let (sender, receiver) = mpsc::sync_channel(0);
    std::thread::Builder::new()
        .name(format!("open-{stem}"))
        .spawn(move || {
            let _pending = pending;
            if let Err(mpsc::SendError(Ok(late))) = sender.send(open()) {
                close_late_open(stem, late);
            }
        })
        .map_err(|error| CaptureError::OpenThreadSpawn {
            stem,
            reason: error.to_string(),
        })?;

    let deadline = Instant::now() + OPEN_LIMIT;
    loop {
        match receiver.recv_timeout(WAIT_STEP) {
            Ok(opened) => return opened,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    // A source offered at the deadline is taken and closed here, with the log.
                    if let Ok(Ok(late)) = receiver.try_recv() {
                        close_late_open(stem, late);
                    }
                    return Err(CaptureError::OpenTimedOut {
                        stem,
                        open_limit: OPEN_LIMIT,
                    });
                }
                while_waiting();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(CaptureError::OpenThreadDied { stem });
            }
        }
    }
}

/// Closes a source whose open finished after the caller gave up. Dropping it closes the device
/// before the stem's pending flag clears.
fn close_late_open<S>(stem: Stem, late: (S, StemReader)) {
    tracing::warn!(capture.stem = %stem, "capture.late_open_closed");
    drop(late);
}
