# Install

Releases are Mac only, for Apple silicon. The app is signed with the self-signed "Meeting Capture
Dev" identity and is not notarized. macOS blocks a browser download of it with "Apple could not
verify". Homebrew and the install script avoid that prompt.

## Install a release

Quit every running copy of Meeting Capture first, dev builds included. A second copy exits at
launch, because the app allows one instance.

### Homebrew

```sh
brew trust --tap kujjwal02/tap
brew install --cask kujjwal02/tap/meeting-capture
```

The cask removes the quarantine flag after it installs the app.

### Install script

```sh
curl -fsSL https://raw.githubusercontent.com/kujjwal02/meeting-capture/main/scripts/install.sh | sh
```

The script downloads the latest DMG, checks its SHA-256, quits a running copy, installs the app in
`/Applications` and opens it. `curl` sets no quarantine flag. To pick a release, set
`MEETING_CAPTURE_VERSION=0.1.0` before `sh`.

### DMG by hand

1. Download `Meeting-Capture_<version>_aarch64.dmg` and its `.sha256` file from the
   [GitHub releases](https://github.com/kujjwal02/meeting-capture/releases).
2. Check the download. The command prints `OK`:

   ```sh
   cd ~/Downloads && shasum -a 256 -c Meeting-Capture_0.1.0_aarch64.dmg.sha256
   ```

3. Clear the quarantine flag on the DMG. The app copied out of it inherits the flag:

   ```sh
   xattr -d com.apple.quarantine ~/Downloads/Meeting-Capture_0.1.0_aarch64.dmg
   ```

4. Open the DMG and drag Meeting Capture to Applications.

### After any install

1. In the tray menu, turn "Start at login" off, then on. The LaunchAgent stores the app's path, so
   this points it at `/Applications`.
2. Log out and back in. The tray icon appears with no Dock icon.

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

- Homebrew: `brew upgrade --cask meeting-capture`. It quits the app first.
- Install script: run the same command again.
- By hand: quit the app from the tray, then install the new DMG and replace the old app.

Settings, sign-ins and the job store stay in `~/Library/Application Support/
com.ujjwalkumar.meetingcapture`. The Accessibility grant survives an upgrade while the signing
identity stays the same.

## The signing identity

`just signing-cert` imports the private key as non-exportable, so it cannot be backed up. If the
login keychain loses it, run `just signing-cert` again. The next install then needs the
Accessibility grant again.

## Uninstall

1. Turn off "Start at login" in the tray.
2. Remove the app. With Homebrew, run `brew uninstall --cask meeting-capture`. Otherwise quit the
   app and delete `/Applications/Meeting Capture.app`.
3. To remove settings, sign-ins, the job store and logs, run
   `brew uninstall --zap --cask meeting-capture`, or delete
   `~/Library/Application Support/com.ujjwalkumar.meetingcapture`. Notes in the vault stay.
