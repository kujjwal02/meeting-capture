//! What every capture process shares about a call folder: the lock that says which process may
//! write its media, and the walk over the app's `calls/` folder.
//!
//! Each capture process has its own lock file, such as `audio.lock` and later `video.lock`. A
//! recording holds it for its whole life and repair holds it while it works, so repair never
//! touches files a live writer owns.
//!
//! The lock is an OS file lock (`flock` on Unix, `LockFileEx` on Windows), not a pid check. The
//! kernel drops it when its holder exits for any reason, `kill -9` included, so a dead writer
//! never leaves a folder locked and a reused pid never makes one look alive. The file itself
//! stays in the folder: deleting it while another process waits on it would let two holders in.
//! It holds the holder's pid for people reading the folder. Only Unix lets them read it while it
//! is held: `LockFileEx` also blocks other processes' reads.

use std::fs::{File, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Released on drop, or by the kernel when the process dies.
pub struct FolderLock {
    _file: File,
}

impl FolderLock {
    /// Takes the lock in `folder/lock_file_name`, or `None` when another process holds it.
    ///
    /// # Errors
    ///
    /// Fails when the lock file cannot be opened or created, or the OS lock call fails.
    pub fn try_take(folder: &Path, lock_file_name: &str) -> std::io::Result<Option<Self>> {
        let path = folder.join(lock_file_name);
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let pid_written = file
            .set_len(0)
            .and_then(|()| writeln!(file, "{}", std::process::id()));
        if let Err(error) = pid_written {
            // The pid is for people; the lock alone decides.
            tracing::warn!(file.path = %path.display(), exception.message = %error, "lock.pid_not_written");
        }
        Ok(Some(Self { _file: file }))
    }

    /// Takes the lock, waiting up to `limit` for its holder to let go.
    ///
    /// # Errors
    ///
    /// As `try_take`.
    pub fn take_within(
        folder: &Path,
        lock_file_name: &str,
        limit: Duration,
    ) -> std::io::Result<Option<Self>> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(lock) = Self::try_take(folder, lock_file_name)? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(RETRY_INTERVAL);
        }
    }
}

/// Every call folder in `calls_dir`, sorted. A missing `calls_dir` has none.
///
/// # Errors
///
/// Fails when `calls_dir` exists but cannot be listed.
pub fn call_folders(calls_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(calls_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            folders.push(entry.path());
        }
    }
    folders.sort();
    Ok(folders)
}
