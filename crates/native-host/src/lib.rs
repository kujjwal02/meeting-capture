//! Host mode for the mc-meet extension. Chrome starts the app binary with the extension's origin
//! as its first argument; `run` then relays native messaging frames between Chrome's stdio and
//! the tray app's socket, and never starts Tauri. Stdout carries frames to Chrome, so nothing
//! else in the process may write to it.

pub mod frames;
pub mod manifest;
pub mod socket;

use std::io;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use tokio::io::AsyncWrite;
use tracing::Instrument as _;

use crate::frames::{LengthOrder, read_frame, write_frame};

/// The native messaging host name the extension connects to.
pub const HOST_NAME: &str = "com.meetingcapture.app";

/// The only origin allowed to start the host. The `key` in the extension's manifest pins the id.
pub const EXTENSION_ORIGIN: &str = "chrome-extension://pbpjeihgfeefjehjnjknnefomhkblcfk/";

const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const APP_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether the process was started by a browser as a native messaging host, from its first
/// argument.
#[must_use]
pub fn is_host_mode(first_argument: Option<&str>) -> bool {
    first_argument.is_some_and(|argument| argument.starts_with("chrome-extension://"))
}

/// Relays frames between Chrome and the app at `socket` until either side closes. Starts the
/// app when the socket is absent, and tells Chrome `app_unavailable` when it does not answer in
/// 10 s.
#[must_use]
pub fn run(origin: &str, socket: &Path) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(exception.message = %error, "could not start the host runtime");
            return ExitCode::FAILURE;
        }
    };
    let span = tracing::info_span!("host.relay", server.address = %socket.display());
    let relayed = runtime.block_on(relay(origin, socket).instrument(span));
    // Stdin is read on a blocking thread that only returns with the next frame.
    runtime.shutdown_background();
    match relayed {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(exception.message = %error, "host relay stopped");
            ExitCode::FAILURE
        }
    }
}

async fn relay(origin: &str, socket: &Path) -> io::Result<()> {
    let mut to_chrome = tokio::io::stdout();
    let Some(connection) = connect_or_start_app(socket).await else {
        tracing::warn!("the app did not start in time");
        let unavailable = serde_json::json!({ "type": "app_unavailable" });
        return send_json(&mut to_chrome, LengthOrder::Native, &unavailable).await;
    };
    let (mut from_app, mut to_app) = tokio::io::split(connection);
    let hello = serde_json::json!({
        "type": "host_hello",
        "origin": origin,
        "host_version": env!("CARGO_PKG_VERSION"),
    });
    send_json(&mut to_app, LengthOrder::Little, &hello).await?;
    tracing::info!("relaying");

    let mut from_chrome = tokio::io::stdin();
    let chrome_to_app = async {
        while let Some(frame) = read_frame(&mut from_chrome, LengthOrder::Native).await? {
            write_frame(&mut to_app, LengthOrder::Little, &frame).await?;
        }
        tracing::info!("Chrome closed the port");
        io::Result::Ok(())
    };
    let app_to_chrome = async {
        while let Some(frame) = read_frame(&mut from_app, LengthOrder::Little).await? {
            write_frame(&mut to_chrome, LengthOrder::Native, &frame).await?;
        }
        tracing::info!("the app closed the connection");
        io::Result::Ok(())
    };
    tokio::select! {
        relayed = chrome_to_app => relayed,
        relayed = app_to_chrome => relayed,
    }
}

async fn send_json<W: AsyncWrite + Unpin>(
    writer: &mut W,
    order: LengthOrder,
    message: &serde_json::Value,
) -> io::Result<()> {
    let frame = serde_json::to_vec(message).map_err(io::Error::other)?;
    write_frame(writer, order, &frame).await
}

async fn connect_or_start_app(socket: &Path) -> Option<socket::HostEnd> {
    let deadline = Instant::now() + APP_START_TIMEOUT;
    let mut is_app_started = false;
    loop {
        match socket::connect(socket).await {
            Ok(connection) => return Some(connection),
            Err(error) => {
                if !is_app_started {
                    tracing::info!(exception.message = %error, "app not reachable; starting it");
                    if let Err(error) = start_app() {
                        tracing::error!(exception.message = %error, "could not start the app");
                        return None;
                    }
                    is_app_started = true;
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
    }
}

// Detached and with no stdio, so the app outlives this process and never writes into Chrome's
// pipe.
fn start_app() -> io::Result<()> {
    let executable = std::env::current_exe()?;
    #[cfg(target_os = "macos")]
    if let Some(bundle) = executable.ancestors().find(|folder| {
        folder
            .extension()
            .is_some_and(|extension| extension == "app")
    }) {
        // `open` starts the bundle through Launch Services, as a click would, so the app gets
        // its own environment; a dev home has to be passed on.
        let mut open = Command::new("open");
        open.arg("-g");
        if let Some(home) = std::env::var_os("MEETING_CAPTURE_HOME") {
            let mut assignment = std::ffi::OsString::from("MEETING_CAPTURE_HOME=");
            assignment.push(home);
            open.arg("--env").arg(assignment);
        }
        let status = open
            .arg(bundle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("open exited with {status}")))
        };
    }
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Chrome may signal the host's process group when the port closes.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(drop)
}
