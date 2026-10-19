//! The app socket: a Unix socket on macOS, a named pipe on Windows. Only the local user reaches
//! it.

use std::io;
use std::path::Path;

/// The app's end of one host-mode connection.
#[cfg(unix)]
pub type AppEnd = tokio::net::UnixStream;
/// The app's end of one host-mode connection.
#[cfg(windows)]
pub type AppEnd = tokio::net::windows::named_pipe::NamedPipeServer;

/// Host mode's end of the connection.
#[cfg(unix)]
pub type HostEnd = tokio::net::UnixStream;
/// Host mode's end of the connection.
#[cfg(windows)]
pub type HostEnd = tokio::net::windows::named_pipe::NamedPipeClient;

/// Connects host mode to the app.
///
/// # Errors
///
/// Returns the connect error: the socket is absent, stale or busy.
#[cfg(unix)]
pub async fn connect(socket: &Path) -> io::Result<HostEnd> {
    tokio::net::UnixStream::connect(socket).await
}

/// Connects host mode to the app.
///
/// # Errors
///
/// Returns the connect error: the pipe is absent or busy.
#[cfg(windows)]
pub async fn connect(socket: &Path) -> io::Result<HostEnd> {
    // Every instance is busy for the moment between the app's accept and its next instance.
    const ERROR_PIPE_BUSY: i32 = 231;
    let open = || tokio::net::windows::named_pipe::ClientOptions::new().open(socket);
    match open() {
        Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            open()
        }
        opened => opened,
    }
}

/// The app's listener for host-mode connections.
pub struct CallListener {
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    #[cfg(windows)]
    pipe_name: std::path::PathBuf,
    // The pipe instance the next client connects to.
    #[cfg(windows)]
    next_end: AppEnd,
}

impl CallListener {
    /// Listens at `socket`. On macOS a socket file left by an app that did not quit cleanly is
    /// removed first, and the new one is readable by its owner only.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the bind.
    #[cfg(unix)]
    pub fn bind(socket: &Path) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        match std::fs::remove_file(socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = tokio::net::UnixListener::bind(socket)?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { listener })
    }

    /// Listens at the pipe named `socket`, for local clients only. Fails when another process
    /// already owns the pipe.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the pipe creation.
    #[cfg(windows)]
    pub fn bind(socket: &Path) -> io::Result<Self> {
        let next_end = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(socket)?;
        Ok(Self {
            pipe_name: socket.to_path_buf(),
            next_end,
        })
    }

    /// Waits for the next host-mode connection.
    ///
    /// # Errors
    ///
    /// Returns the accept error.
    #[cfg(unix)]
    pub async fn accept(&mut self) -> io::Result<AppEnd> {
        let (connection, _) = self.listener.accept().await?;
        Ok(connection)
    }

    /// Waits for the next host-mode connection.
    ///
    /// # Errors
    ///
    /// Returns the connect error, or the error creating the next pipe instance.
    #[cfg(windows)]
    pub async fn accept(&mut self) -> io::Result<AppEnd> {
        self.next_end.connect().await?;
        let next_end = tokio::net::windows::named_pipe::ServerOptions::new()
            .reject_remote_clients(true)
            .create(&self.pipe_name)?;
        Ok(std::mem::replace(&mut self.next_end, next_end))
    }
}
