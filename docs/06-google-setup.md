# Google setup

Sign-in needs one OAuth client in a personal Google Cloud project. The repo is public, so the
client stays out of it: you load it in the settings window.

## Create the client

1. Open <https://console.cloud.google.com/projectcreate> with the personal account. Name the
   project `meeting-capture`.
2. Enable the Google Calendar API: <https://console.cloud.google.com/apis/library/calendar-json.googleapis.com>.
   Enable the Google Drive API too: <https://console.cloud.google.com/apis/library/drive.googleapis.com>.
3. Open Google Auth Platform, then Branding. Set the app name to "Meeting Capture" and add your
   email as the support and developer contact.
4. Under Audience, pick "External". Then press "Publish app" so the status reads "In production".
   A client in "Testing" expires refresh tokens after 7 days.
5. Under Data Access, add the scopes `.../auth/calendar.events.readonly`, `.../auth/drive.file`,
   `openid` and `.../auth/userinfo.email`. `drive.file` is a non-sensitive scope: the app sees
   only the files and folders it created.
6. Under Clients, create a client of type "Desktop app". Download its JSON file.

## Load it in the app

1. In the tray menu, open Settings.
2. Under "Google OAuth client", load the JSON file, then press Save. The client ID and the
   secret go in `settings.toml`.
3. Press "Add Google account". Sign in with akaiketech.com first, so it wins tied matches.
4. Google warns that the app is unverified. Press Advanced, then "Go to Meeting Capture".
   Tick the calendar box on the consent screen; without it the app refuses the sign-in.
5. Press "Add Google account" again for the personal account.

Delete the downloaded JSON file afterwards.

Saving a new client ID removes the old secret and signs every account out, the Drive account
included, since Google ties both to the client. Enter the new secret, add the accounts again and
run `mc drive sign-in`.

## Drive account

Call media can go to one Google Drive account. It signs in on its own for `drive.file` only, apart
from the calendar accounts. It may be one of the calendar accounts or another account. Google
keeps one grant per account and client: on a shared account, revoking the calendar or the Drive
token ends both. The app revokes a token only when no other stored token has its account.

1. Load the OAuth client first, as above.
2. In Settings, under "Media archive", pick the account in "Drive account", or "Add another
   account…". Tick the Drive box on the consent screen. `mc drive sign-in` does the same from a
   terminal.
3. The account and its refresh token go in `[drive]` in `settings.toml`. Uploads go under
   "Folder in My Drive", "Meeting capture" by default.

## Turn on the media archive

1. In Settings, under "Media archive", tick "Archive call media to Google Drive".
2. Sign the Drive account in, as above, if the status line asks for it.
3. Press Save. The first sweep starts at once, then one runs every hour.

An hour after a call ends and its note is written, its `call.mp4` and stems go to Drive, and the
local copies are deleted. Notes then link the Drive copies. The tray shows how many calls are
archived. "Drive full" or "Drive: sign in again" turns the icon amber and pauses the archive
until the next hourly check; sign in again from Settings to go on at once. Run
`mc restore <meeting ID>` to bring a call's media back, such as before `mc transcribe`.

`mc drive upload <file> [--folder a/b]` and `mc drive download <id> <dest>` move one file. An
upload prints Drive's MD5 and whether it matches the local file.

## Upgrading from a keychain build

Secrets live in `settings.toml`, and the app no longer reads the keychain. Enter the client
secret and the API key again, then sign in to each account. Delete the old
`com.ujjwalkumar.meetingcapture*` items in Keychain Access.

## Work account

The akaiketech.com Workspace admin may block unverified third-party apps. If sign-in fails with
"access blocked", ask the admin to trust the client ID under Security, API controls. The other
way out is a second OAuth client with an "Internal" consent screen in the akaiketech.com org.
