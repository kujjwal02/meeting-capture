//! The capture helper. The app spawns it, so a crash or a hung Core Audio call never takes the
//! tray down. `probe` records the stems at their native rates and reports what the devices did.

#[cfg(target_os = "macos")]
mod probe;

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record the stems at native rates and print one status line per second.
    Probe {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// Folder for `mic-native.wav` and `remote-native.wav`.
        #[arg(long)]
        out: PathBuf,
        #[arg(long, value_enum, default_value_t = Sources::Both)]
        sources: Sources,
        #[arg(long, value_enum, default_value_t = MicChoice::AvoidBluetooth)]
        mic: MicChoice,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Sources {
    Both,
    Mic,
    Remote,
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
        Command::Probe {
            seconds,
            out,
            sources,
            mic,
        } => {
            #[cfg(target_os = "macos")]
            return probe::probe(Duration::from_secs(seconds), &out, sources, mic);
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (seconds, out, sources, mic);
                anyhow::bail!("probe records system audio on macOS only")
            }
        }
    }
}
