//! `capture-check`: runs each `MC_CAPTURE_INJECT` fault through a debug `mc-capture` helper,
//! feeds the helper's reports to the detectors as the app does, and checks that the expected
//! problem is raised, the recorders dumped, and `mc debug replay` agrees. macOS only: the
//! helpers record through Core Audio and ScreenCaptureKit.

#[cfg(target_os = "macos")]
mod cases;
#[cfg(target_os = "macos")]
mod checks;
#[cfg(target_os = "macos")]
mod driver;
#[cfg(target_os = "macos")]
mod run;

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    return run::main();
    #[cfg(not(target_os = "macos"))]
    {
        println!("capture-check runs on macOS only");
        std::process::ExitCode::SUCCESS
    }
}
