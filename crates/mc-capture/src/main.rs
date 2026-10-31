//! The capture helper. The app spawns it, so a crash or a hung Core Audio call never takes the
//! tray down. `audio` and `video` are the processes the app supervises: commands on stdin,
//! reports on stdout. `windows` lists the windows `video` can record.
//! `probe` records the stems at their native rates and reports what the devices did; `record`
//! writes one segment of a call folder through the stem writer; `devices` lists the Core Audio
//! devices; `await-device-change` exits on the first device change; `repair` recovers the
//! segments a dead writer left open in a call folder, audio and video, as the app does at launch;
//! `finalize` writes a call folder's `call.mp4`, as the app does when a call ends and at launch
//! for each folder `pending-finalize` lists; `purge-media` deletes the video of calls older than
//! 90 days, as the app does at launch and once a day.

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod devices;
#[cfg(all(target_os = "macos", debug_assertions))]
mod inject;
#[cfg(target_os = "macos")]
mod probe;
#[cfg(target_os = "macos")]
mod record;
#[cfg(target_os = "macos")]
mod video;

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

/// What a JSON line on stdout reports.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum LineKind {
    Status,
    Summary,
    DeviceChanged,
    MicUnavailable,
    Permission,
    SystemSleep,
    SystemWake,
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the app: JSON line commands on stdin (`start`, `stop`), reports on stdout. Exits
    /// when stdin closes.
    Audio {
        #[arg(long, value_enum, default_value_t = MicChoice::AvoidBluetooth)]
        mic: MicChoice,
    },
    /// Serve the app's video: JSON line commands on stdin (`start` with a window hint, `stop`),
    /// reports on stdout. Records one window through ScreenCaptureKit. Exits when stdin closes.
    Video,
    /// Print every window ScreenCaptureKit lists, on and off screen, as one JSON array, for
    /// picking a `window_id` by hand.
    Windows {
        /// Only windows of this app.
        #[arg(long)]
        bundle_id: Option<String>,
    },
    /// Record the stems at native rates and print one status line per second.
    Probe {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// Folder for `mic-native.wav` and `remote-native.wav`.
        #[arg(long)]
        out: PathBuf,
        #[arg(long, value_enum, default_value_t = MicChoice::AvoidBluetooth)]
        mic: MicChoice,
    },
    /// Record the folder's next segment as 16 kHz stems placed by host time, with
    /// `audio.json`. Prints one status line per second and a summary.
    Record {
        /// The call folder.
        #[arg(long)]
        folder: PathBuf,
        /// The segment number. Default: one past the folder's highest.
        #[arg(long)]
        segment: Option<u32>,
        /// Default: the folder's name.
        #[arg(long)]
        call_id: Option<String>,
        #[arg(long, value_enum, default_value_t = PlatformChoice::Other)]
        platform: PlatformChoice,
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        #[arg(long, value_enum, default_value_t = MicChoice::AvoidBluetooth)]
        mic: MicChoice,
    },
    /// Print every Core Audio device this process can see, and the mic the rule would record,
    /// as one JSON object. Opens no device. Private aggregates of other processes are not listed.
    Devices {
        #[arg(long, value_enum, default_value_t = MicChoice::AvoidBluetooth)]
        mic: MicChoice,
    },
    /// Wait until the default input, the default output or the device list changes, or the
    /// time runs out. Prints `{"changed":true}` or `{"changed":false}`. The app runs it while
    /// it backs off after a device failed to open, so a new device ends the wait early.
    AwaitDeviceChange {
        #[arg(long, default_value_t = 300)]
        seconds: u64,
    },
    /// Rebuild the headers of the audio segments a dead writer left open in a call folder, end
    /// its open video playlists, and mark both recovered. Leaves each alone while a helper
    /// records there. Prints both outcomes as one JSON object.
    Repair {
        /// The call folder.
        #[arg(long)]
        folder: PathBuf,
    },
    /// Repair the call folder, then write `call.mp4` in it: the video segments as one track and
    /// the stems mixed into one AAC track, each at its host time. Prints the outcome as one JSON
    /// object.
    Finalize {
        /// The call folder.
        #[arg(long)]
        folder: PathBuf,
    },
    /// List the call folders whose `call.mp4` is missing or older than their manifests, and whose
    /// last finalize did not fail on the same manifests: the ones the app finalizes at launch.
    /// Prints one JSON object.
    PendingFinalize {
        /// The folder that holds the call folders.
        #[arg(long)]
        calls_dir: PathBuf,
    },
    /// Delete `call.mp4` and the video fragments of each call that ended more than 90 days ago
    /// and has nothing left to finalize. The stems stay. Prints one JSON object.
    PurgeMedia {
        /// The folder that holds the call folders. Nothing outside it is deleted.
        #[arg(long)]
        calls_dir: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum PlatformChoice {
    Meet,
    Teams,
    Zoom,
    Slack,
    Other,
}

#[derive(Clone, Copy, ValueEnum)]
enum MicChoice {
    FollowDefault,
    AvoidBluetooth,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .json()
        .init();
    match Cli::parse().command {
        Command::Audio { mic } => {
            #[cfg(target_os = "macos")]
            return audio::serve(mic);
            #[cfg(not(target_os = "macos"))]
            {
                let _ = mic;
                anyhow::bail!("audio needs system audio, which only macOS has yet")
            }
        }
        Command::Probe { seconds, out, mic } => {
            #[cfg(target_os = "macos")]
            return probe::probe(Duration::from_secs(seconds), &out, mic);
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (seconds, out, mic);
                anyhow::bail!("probe records system audio on macOS only")
            }
        }
        Command::Record {
            folder,
            segment,
            call_id,
            platform,
            seconds,
            mic,
        } => {
            #[cfg(target_os = "macos")]
            return record::record(&record::RecordRequest {
                folder,
                segment,
                call_id,
                platform: platform.platform(),
                length: Duration::from_secs(seconds),
                mic,
            });
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (folder, segment, call_id, platform, seconds, mic);
                anyhow::bail!("record needs system audio, which only macOS has yet")
            }
        }
        Command::AwaitDeviceChange { seconds } => {
            #[cfg(target_os = "macos")]
            return await_device_change(Duration::from_secs(seconds));
            #[cfg(not(target_os = "macos"))]
            {
                let _ = seconds;
                anyhow::bail!("await-device-change watches Core Audio on macOS only")
            }
        }
        Command::Repair { folder } => {
            let audio = stem_writer::repair_folder(&folder)?;
            let video = video_capture::repair_folder(&folder)?;
            let outcome = serde_json::json!({ "audio": audio, "video": video });
            println!("{}", serde_json::to_string(&outcome)?);
            Ok(())
        }
        Command::Finalize { folder } => finalize(&folder),
        Command::PendingFinalize { calls_dir } => pending_finalize(&calls_dir),
        Command::PurgeMedia { calls_dir } => {
            let purge =
                call_finalize::purge_expired_media(&calls_dir, std::time::SystemTime::now())?;
            println!("{}", serde_json::to_string(&purge)?);
            Ok(())
        }
        Command::Video => {
            #[cfg(target_os = "macos")]
            return video::serve();
            #[cfg(not(target_os = "macos"))]
            anyhow::bail!("video records through ScreenCaptureKit, which only macOS has")
        }
        Command::Windows { bundle_id } => {
            #[cfg(target_os = "macos")]
            return print_windows(bundle_id.as_deref());
            #[cfg(not(target_os = "macos"))]
            {
                let _ = bundle_id;
                anyhow::bail!("windows lists ScreenCaptureKit windows on macOS only")
            }
        }
        Command::Devices { mic } => {
            #[cfg(target_os = "macos")]
            return print_devices(mic.rule());
            #[cfg(not(target_os = "macos"))]
            {
                let _ = mic;
                anyhow::bail!("devices lists Core Audio devices on macOS only")
            }
        }
    }
}

/// Prints the outcome. A failure to lock the folder or write its record is a failed outcome
/// too, so the app always reads one line.
fn finalize(folder: &std::path::Path) -> anyhow::Result<()> {
    let outcome = call_finalize::finalize_folder(folder).unwrap_or_else(|error| {
        call_finalize::FinalizeOutcome::Failed {
            message: error.to_string(),
        }
    });
    println!("{}", serde_json::to_string(&outcome)?);
    Ok(())
}

fn pending_finalize(calls_dir: &std::path::Path) -> anyhow::Result<()> {
    let folders = call_folder::call_folders(calls_dir)?
        .into_iter()
        .filter(|folder| call_finalize::needs_finalize(folder))
        .collect();
    let pending = call_finalize::PendingFinalize { folders };
    println!("{}", serde_json::to_string(&pending)?);
    Ok(())
}

#[cfg(target_os = "macos")]
impl PlatformChoice {
    fn platform(self) -> sources::Platform {
        match self {
            Self::Meet => sources::Platform::Meet,
            Self::Teams => sources::Platform::Teams,
            Self::Zoom => sources::Platform::Zoom,
            Self::Slack => sources::Platform::Slack,
            Self::Other => sources::Platform::Other,
        }
    }
}

#[cfg(target_os = "macos")]
impl MicChoice {
    fn rule(self) -> capture::MicRule {
        match self {
            Self::FollowDefault => capture::MicRule::FollowDefault,
            Self::AvoidBluetooth => capture::MicRule::AvoidBluetooth,
        }
    }
}

#[cfg(target_os = "macos")]
fn print_devices(rule: capture::MicRule) -> anyhow::Result<()> {
    #[derive(serde::Serialize)]
    struct DeviceReport {
        devices: Vec<capture::HalDevice>,
        mic_pick: capture::MicPick,
    }
    let report = DeviceReport {
        devices: capture::hal_devices()?,
        mic_pick: capture::mic_pick(&capture::MicRequest::call_start(rule))?,
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

/// Reads the listeners' flag, which only a HAL callback sets, so the HAL is not polled.
#[cfg(target_os = "macos")]
fn await_device_change(limit: Duration) -> anyhow::Result<()> {
    const CHECK_INTERVAL: Duration = Duration::from_millis(100);
    capture::watch_hal_changes()?;
    let started = std::time::Instant::now();
    let is_changed = loop {
        let changes = capture::take_hal_changes();
        if changes.any() {
            tracing::info!(
                hal.default_input_changed = changes.default_input,
                hal.default_output_changed = changes.default_output,
                hal.device_list_changed = changes.device_list,
                "device.changed"
            );
            break true;
        }
        if started.elapsed() >= limit {
            break false;
        }
        std::thread::sleep(CHECK_INTERVAL);
    };
    println!("{}", serde_json::json!({ "changed": is_changed }));
    Ok(())
}

#[cfg(target_os = "macos")]
fn print_windows(bundle_id: Option<&str>) -> anyhow::Result<()> {
    video_capture::mac::window::connect_window_server();
    let windows = video_capture::mac::window::list_windows().map_err(anyhow::Error::msg)?;
    let windows: Vec<_> = windows
        .into_iter()
        .filter(|window| bundle_id.is_none_or(|wanted| window.bundle_id.as_deref() == Some(wanted)))
        .collect();
    println!("{}", serde_json::to_string(&windows)?);
    Ok(())
}
