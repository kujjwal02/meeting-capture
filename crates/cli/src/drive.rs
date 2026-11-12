//! `mc drive`: signs the Drive account in, and moves single files to and from its Drive.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Subcommand;
use pipeline::drive::{DriveClient, FileId};
use pipeline::google::{self, OAuthClient};
use pipeline::settings::{SettingsError, SettingsFile, SettingsStore};

#[derive(Subcommand)]
pub(crate) enum DriveCommand {
    /// Sign the Drive account in through the browser, for the drive.file scope only. The account
    /// may also be a calendar account.
    SignIn,
    /// Upload a file under the Drive root folder from settings, then compare Drive's MD5 with the local one.
    Upload {
        file: PathBuf,
        /// Folders below the root folder, such as `2026/11`. Missing ones are created.
        #[arg(long)]
        folder: Option<String>,
    },
    /// Download a file the app uploaded, by its Drive file ID.
    Download { id: String, dest: PathBuf },
}

pub(crate) fn run(command: DriveCommand, settings_store: &SettingsStore) -> anyhow::Result<()> {
    match command {
        DriveCommand::SignIn => sign_in(settings_store),
        DriveCommand::Upload { file, folder } => {
            upload(settings_store, &file, folder.as_deref().unwrap_or_default())
        }
        DriveCommand::Download { id, dest } => {
            let downloaded = connect(&settings_store.read()?)?.download(&FileId::new(id), &dest)?;
            println!("name:  {}", downloaded.name);
            println!("size:  {}", describe_size(downloaded.size_bytes));
            // `download` refuses a file whose size or MD5 differs from Drive's.
            println!(
                "md5:   {} (matches Drive's)",
                downloaded.md5_checksum.as_deref().unwrap_or("none")
            );
            println!("saved: {}", dest.display());
            Ok(())
        }
    }
}

fn sign_in(settings_store: &SettingsStore) -> anyhow::Result<()> {
    let file = settings_store.read()?;
    let client = oauth_client(&file)?;
    let signed_in =
        google::sign_in_for_drive(&client, file.drive.account.as_deref(), |auth_url| {
            eprintln!(
                "Sign in to Google Drive in the browser. If no browser opens, open this URL:"
            );
            eprintln!("{auth_url}");
            open_browser(auth_url);
            Ok(())
        })?;
    let email = signed_in.email;
    let refresh_token = signed_in.refresh_token;
    // The edit reads the file again: it may have changed during the minutes in the browser.
    let edited = settings_store.edit(|file| {
        // The token belongs to the client that signed in, and fails with any other.
        if file.google.client_id() != Some(client.client_id.as_str()) {
            return Err(SettingsError::GoogleClientChangedDuringSignIn);
        }
        file.drive.account = Some(email.clone());
        file.drive.refresh_token = Some(refresh_token.clone());
        Ok(())
    });
    let change = match edited {
        Ok(change) => change,
        Err(error) => {
            revoke_unsaved_token(settings_store, &client, &email, &refresh_token);
            return Err(error.into());
        }
    };
    // Holds the old Drive account's token only when no calendar token shares its grant.
    for (old_account, old_token) in &change.tokens_to_revoke {
        google::revoke(old_account, old_token);
    }
    println!("Drive account: {email}");
    Ok(())
}

// A token the file never took would stay valid at Google. A stored token of the same account and
// client shares its grant, and the revoke would end that one too.
fn revoke_unsaved_token(
    settings_store: &SettingsStore,
    client: &OAuthClient,
    email: &str,
    refresh_token: &str,
) {
    match settings_store.read() {
        Ok(file)
            if file.google.client_id() != Some(client.client_id.as_str())
                || !file.has_token_for(email) =>
        {
            google::revoke(email, refresh_token);
        }
        Ok(_) => {}
        Err(error) => eprintln!(
            "could not read the settings to revoke the new Drive grant; remove Meeting Capture at \
             myaccount.google.com/connections if it is unused: {error}"
        ),
    }
}

fn upload(settings_store: &SettingsStore, local_path: &Path, folder: &str) -> anyhow::Result<()> {
    let file = settings_store.read()?;
    let drive_client = connect(&file)?;
    let folder_names: Vec<&str> = std::iter::once(file.drive.root_folder.as_str())
        .chain(folder.split('/').filter(|name| !name.trim().is_empty()))
        .collect();
    let folder_id = drive_client.ensure_folder_path(&folder_names)?;
    let verified = drive_client.upload(local_path, &folder_id, None, |_session| {})?;
    let uploaded = verified.file();
    println!("id:    {}", uploaded.id);
    println!("size:  {}", describe_size(uploaded.size_bytes));
    println!("md5:   {} (matches the local file)", verified.md5());
    println!(
        "link:  {}",
        uploaded.web_view_link.as_deref().unwrap_or("none")
    );
    Ok(())
}

pub(crate) fn connect(file: &SettingsFile) -> anyhow::Result<DriveClient> {
    file.drive.validate()?;
    let client = oauth_client(file)?;
    let (Some(account), Some(refresh_token)) = (&file.drive.account, &file.drive.refresh_token)
    else {
        anyhow::bail!("no Drive account is signed in; run `mc drive sign-in`");
    };
    Ok(DriveClient::connect(
        client,
        account.clone(),
        refresh_token.clone(),
    )?)
}

fn oauth_client(file: &SettingsFile) -> anyhow::Result<OAuthClient> {
    OAuthClient::from_settings(&file.google)?
        .context("no Google OAuth client is set; load it in the settings window first")
}

fn describe_size(size_bytes: Option<u64>) -> String {
    size_bytes.map_or_else(|| "unknown".to_owned(), |size| format!("{size} bytes"))
}

// The URL is printed as well, so a failed open only costs a copy and paste.
fn open_browser(auth_url: &str) {
    #[cfg(target_os = "macos")]
    let opened = std::process::Command::new("open").arg(auth_url).status();
    // `start` goes through cmd.exe, which splits the URL at each '&'.
    #[cfg(windows)]
    let opened = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", auth_url])
        .status();
    #[cfg(not(any(target_os = "macos", windows)))]
    let opened: std::io::Result<std::process::ExitStatus> =
        Err(std::io::Error::other("no browser opener on this OS"));
    if let Err(error) = opened {
        eprintln!("could not open the browser: {error}");
    }
}
