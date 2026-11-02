//! The command line, the run's telemetry, and one result row per case.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use capture_protocol::WindowHint;
use clap::Parser;
use detectors::{Host, LineQueue};
use serde_json::Value;

use crate::cases::{self, Case, Helper};
use crate::checks::{CheckTools, check_case};
use crate::driver::{CasePaths, Setup, run_case};

const FINDER_BUNDLE_ID: &str = "com.apple.finder";
/// Finder needs a moment to list the window it opened.
const WINDOW_OPEN_WAIT: Duration = Duration::from_secs(2);

#[derive(Parser)]
#[command(
    about = "Run MC_CAPTURE_INJECT faults through a debug mc-capture and check the detectors"
)]
struct Cli {
    /// A debug build of `mc-capture`.
    #[arg(long)]
    helper: PathBuf,
    /// The `mc` binary, for the replay check.
    #[arg(long)]
    mc: PathBuf,
    /// The run's folder: logs, call folders, debug folders and replay bundles.
    #[arg(long)]
    out: PathBuf,
    /// The cases to run. None runs them all.
    cases: Vec<String>,
}

pub(crate) fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("capture-check: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<bool> {
    let chosen = cases::chosen(&cli.cases)?;
    let log_dir = cli.out.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    telemetry::init(telemetry::TelemetryConfig {
        service: telemetry::Service::CaptureCheck,
        service_version: env!("CARGO_PKG_VERSION"),
        log_sink: telemetry::LogSink::DailyFiles {
            dir: log_dir.clone(),
        },
        panic_dump_dir: Some(cli.out.join("debug")),
    })?;
    let lines = Arc::new(LineQueue::new());
    telemetry::install_line_tap(lines.clone());
    let has_video_case = chosen.iter().any(|case| case.helper == Helper::Video);
    let mut setup = Setup {
        helper_path: cli.helper.clone(),
        out: cli.out.clone(),
        lines,
        host: Host::new(),
        window_hint: if has_video_case {
            Some(finder_window(&cli.helper)?)
        } else {
            None
        },
    };
    let tools = CheckTools {
        mc_path: &cli.mc,
        log_dir: &log_dir,
    };
    println!(
        "capture-check: {} case(s), output in {}",
        chosen.len(),
        cli.out.display()
    );
    let mut failed_count = 0;
    for case in &chosen {
        let outcome = run_and_check(&mut setup, &tools, case);
        let row = match &outcome {
            Ok(None) => "pass".to_owned(),
            Ok(Some(failure)) => format!("fail: {failure}"),
            Err(error) => format!("fail: {error:#}"),
        };
        if !matches!(outcome, Ok(None)) {
            failed_count += 1;
        }
        println!("{:<26} {row}", case.name);
    }
    println!(
        "{} passed, {failed_count} failed",
        chosen.len() - failed_count
    );
    Ok(failed_count == 0)
}

fn run_and_check(
    setup: &mut Setup,
    tools: &CheckTools<'_>,
    case: &Case,
) -> anyhow::Result<Option<String>> {
    let run = run_case(setup, case)?;
    check_case(tools, case, &CasePaths::of(&setup.out, case), &run)
}

/// Opens a Finder window on the home folder and returns its hint, as the huddle watcher hands
/// one to the video helper.
fn finder_window(helper: &Path) -> anyhow::Result<WindowHint> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Command::new("open").arg(home).status()?;
    std::thread::sleep(WINDOW_OPEN_WAIT);
    let output = Command::new(helper)
        .args(["windows", "--bundle-id", FINDER_BUNDLE_ID])
        .output()?;
    let windows: Vec<Value> = serde_json::from_slice(&output.stdout)
        .context("mc-capture windows printed no window list")?;
    let window = windows
        .iter()
        .find(|window| {
            window.get("is_on_screen") == Some(&Value::Bool(true))
                && window.get("layer").and_then(Value::as_i64) == Some(0)
        })
        .context("no on-screen Finder window; is Screen Recording allowed for this terminal?")?;
    let window_id = window
        .get("window_id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .context("the Finder window has no window_id")?;
    Ok(WindowHint {
        bundle_id: FINDER_BUNDLE_ID.to_owned(),
        title: window
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned),
        pid: window
            .get("pid")
            .and_then(Value::as_i64)
            .and_then(|pid| i32::try_from(pid).ok()),
        window_id,
    })
}
