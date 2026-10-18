# Install

Releases are Mac only, for Apple silicon. The app is signed with the self-signed "Meeting Capture
Dev" identity and is not notarized, so macOS blocks it until you clear the download flag.

## Install a release

1. Download `Meeting-Capture_<version>_aarch64.dmg` and its `.sha256` file from the
   [GitHub releases](https://github.com/kujjwal02/meeting-capture/releases).
2. Check the download. The command prints `OK`:

   ```sh
   cd ~/Downloads && shasum -a 256 -c Meeting-Capture_0.1.0_aarch64.dmg.sha256
   ```

3. Clear the download flag on the DMG. macOS blocks a non-notarized DMG with "Apple could not
   verify", and the app copied out of it inherits the flag:

   ```sh
   xattr -d com.apple.quarantine ~/Downloads/Meeting-Capture_0.1.0_aarch64.dmg
   ```

4. Quit every running copy of Meeting Capture, dev builds included. A second copy exits at launch,
   because the app allows one instance.
5. Open the DMG and drag Meeting Capture to Applications.
6. Open the app. If macOS still blocks it, open System Settings, then Privacy & Security, and press
   "Open Anyway" under Security.
7. In the tray menu, turn "Start at login" off, then on. The LaunchAgent stores the app's path, so
   this points it at `/Applications`.
8. Log out and back in. The tray icon appears with no Dock icon.

## First-time setup

Skip this on a Mac that already ran a dev build: the installed app reads the same settings and job
store.

1. Grant Accessibility when macOS asks. Slack huddle captions need it. The grant is in System
   Settings, Privacy & Security, Accessibility.
2. Load a Google OAuth client and sign in. Follow [Google setup](06-google-setup.md).
3. Pick the LLM provider in Settings. See [LLM extraction](07-llm-extraction.md).
4. Install the TranscripTonic Chrome extension. Set its webhook to
   `http://127.0.0.1:47615/transcriptonic` and choose the "advanced" body.
5. Take a Meet call. The note appears in `~/knowledge` after the call ends.

## Upgrade

1. Quit Meeting Capture from the tray.
2. Install the new DMG as above. Replace the old app when Finder asks.

Settings, sign-ins and the job store stay in `~/Library/Application Support/
com.ujjwalkumar.meetingcapture`. The Accessibility grant survives an upgrade while the signing
identity stays the same.

## The signing identity

`just signing-cert` imports the private key as non-exportable, so it cannot be backed up. If the
login keychain loses it, run `just signing-cert` again. The next install then needs the
Accessibility grant again.

## Uninstall

1. Turn off "Start at login" in the tray, then quit.
2. Delete `/Applications/Meeting Capture.app`.
3. To remove settings, sign-ins, the job store and logs, delete
   `~/Library/Application Support/com.ujjwalkumar.meetingcapture`. Notes in the vault stay.
