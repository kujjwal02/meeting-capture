//! `mc debug`: reads a saved debug report, or any folder or file of telemetry lines, with no app
//! running and no app data folder.

mod bundle;
mod otlp;
mod timeline;

use std::path::PathBuf;

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum DebugCommand {
    /// Print every line of a report on one clock, in IST.
    Timeline {
        /// A debug report folder, a debug folder or one `.jsonl` file.
        bundle: PathBuf,
        /// One row per minute: speech, levels, caption frames, acks, presses and problems.
        #[arg(long)]
        by_minute: bool,
        /// Only lines of this service, such as `app`, `mc-capture` or `mc-meet`.
        #[arg(long)]
        service: Option<String>,
    },
    /// Write the report as OTLP/JSON lines for an OpenTelemetry Collector's `otlpjsonfile`
    /// receiver.
    ExportOtlp {
        /// A debug report folder, a debug folder or one `.jsonl` file.
        bundle: PathBuf,
        /// The file to write, one export request per line.
        #[arg(long)]
        out: PathBuf,
    },
}

pub(crate) fn run(command: DebugCommand) -> anyhow::Result<()> {
    match command {
        DebugCommand::Timeline {
            bundle,
            by_minute,
            service,
        } => {
            let bundle = bundle::Bundle::read(&bundle)?;
            let stdout = std::io::stdout().lock();
            let printed = if by_minute {
                timeline::print_by_minute(&bundle, service.as_deref(), stdout)
            } else {
                timeline::print_lines(&bundle, service.as_deref(), stdout)
            };
            match printed {
                // A pager or `head` that stops reading has all it wanted.
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                other => other?,
            }
        }
        DebugCommand::ExportOtlp { bundle, out } => {
            let bundle = bundle::Bundle::read(&bundle)?;
            let written = otlp::export(&bundle, &out)?;
            println!(
                "wrote {} export requests ({} logs, {} spans, {} metric points) to {}",
                written.requests,
                written.log_records,
                written.spans,
                written.metric_points,
                out.display()
            );
        }
    }
    Ok(())
}
