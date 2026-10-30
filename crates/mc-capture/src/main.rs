//! The capture helper. The app spawns it, so a crash or a hung Core Audio call never takes the
//! tray down. `audio` is the process the app supervises: commands on stdin, reports on stdout.
//! `probe` records the stems at their native rates and reports what the devices did; `record`
//! writes one segment of a call folder through the stem writer; `devices` lists the Core Audio
//! devices.

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod probe;
#[cfg(target_os = "macos")]
mod record;

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
        mic_pick: capture::mic_pick(rule)?,
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
