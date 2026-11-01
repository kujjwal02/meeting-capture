//! Opens what the tray points at. Each target names the app that shows it, because the default
//! app for `.md` can be anything, even a drawing app.

use std::path::Path;

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt as _;

pub(crate) enum LaunchTarget<'a> {
    /// A vault note: Obsidian, else the text editor.
    Note(&'a Path),
    /// A file to find on disk: Finder or Explorer, with the file selected.
    RevealFile(&'a Path),
}

pub(crate) fn launch(app: &AppHandle, target: &LaunchTarget<'_>) {
    let (path, launched) = match target {
        LaunchTarget::Note(path) => (path, open_in_obsidian(app, path)),
        LaunchTarget::RevealFile(path) => (
            path,
            app.opener()
                .reveal_item_in_dir(path)
                .map_err(|error| error.to_string()),
        ),
    };
    if let Err(failure) = launched {
        // A note's file name is the meeting's title: `note.path` is a key a debug report
        // without text shortens.
        match target {
            LaunchTarget::Note(_) => tracing::error!(
                note.path = %path.display(),
                exception.message = failure,
                "could not open"
            ),
            LaunchTarget::RevealFile(_) => tracing::error!(
                file.path = %path.display(),
                exception.message = failure,
                "could not open"
            ),
        }
    }
}

fn open_in_obsidian(app: &AppHandle, path: &Path) -> Result<(), String> {
    let mut uri = tauri::Url::parse("obsidian://open").map_err(|error| error.to_string())?;
    uri.query_pairs_mut()
        .append_pair("path", &path.to_string_lossy());
    match app.opener().open_url(uri.as_str(), None::<&str>) {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::warn!(
                exception.message = %error,
                "Obsidian did not open; using the text editor"
            );
            open_in_text_editor(path)
        }
    }
}

fn open_in_text_editor(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return match std::process::Command::new("open")
        .arg("-t")
        .arg(path)
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("open -t exited with {status}")),
        Err(error) => Err(error.to_string()),
    };
    #[cfg(not(target_os = "macos"))]
    std::process::Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
