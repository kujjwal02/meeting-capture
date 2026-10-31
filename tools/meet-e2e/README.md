# Meet end-to-end run

`just meet-e2e` runs one real Google Meet call through mc-meet and a dev build of the app. It then
checks the call log and the note. It runs on macOS only.

## What it needs

- Node 24 or later, and Chrome.
- A patchrome debug profile signed in to a Google account, the host. The default name is `mcdev`.
  Create it once with `npx -y patchrome@latest profile create mcdev --mode debug`, then sign in
  with `npx -y patchrome@latest --profile mcdev login https://meet.google.com`.
- A patchrome profile signed in to a second Google account, the guest. The default is patchrome's
  `default` profile.
- The host account must not be in a call on another device.

Set `MEET_E2E_HOST_PROFILE` or `MEET_E2E_GUEST_PROFILE` to use other profiles. Set
`MEET_E2E_GUEST_ANONYMOUS=1` for a guest profile that is not signed in: it types a name before
it asks to join. Only some Workspace domains let a signed-out guest in, so try one join by hand
first. Set
`MEET_E2E_DIR` to move the work folder from `~/.cache/meeting-capture-e2e`.

The run puts the dev binary in `Meeting Capture Dev.app` (bundle id
`com.ujjwalkumar.meetingcapture.dev`), so macOS lists it apart from the installed app. It signs the
bundle with the `Meeting Capture Dev` identity, so the Accessibility grant survives rebuilds. Create
the identity once with
`scripts/create-signing-cert.sh "Meeting Capture Dev" ~/.tauri/meeting-capture-dev-signing.p12`, or
set `MEET_E2E_SIGNING_IDENTITY` to use another.

## What it does

1. Copies the dev binary and `mc-capture` into the work folder and starts the app with its own
   home and vault. The app records the call's audio with `mc-capture` and the built-in mic.
2. Writes a `com.meetingcapture.app` host manifest into the host browser's data folder. Chrome
   reads it before the global one, so only the host browser starts the dev app.
3. Loads `extension/` into the host browser and gives the host a fake microphone that reads a
   `say` recording.
4. The host opens a new meeting. The guest asks to join, muted, and the host admits it from the
   toast or the People panel. The guest sends a chat message. Both pages keep their media
   elements muted, so no call audio plays out loud.
5. The run sends `stop_recording` and then `resume_recording` the way mc-meet's popup does. A
   script cannot click the tray menu, since it is not in the accessibility tree.
6. Both leave. The run waits for `call_ended` in the call log and for the note in the vault.

The dev app rewrites the global host manifests of Chrome, Chromium, Brave and Edge at start. The
run restores them and the host browser's manifest at the end, also after a failure. While it runs,
mc-meet in your own browsers starts the dev binary without the dev home.

## What it checks

- The log has one session, starting with `call_started` and ending with `call_ended`.
- The log has at least 3 captions, all from the data channel.
- A roster lists both people.
- The chat time is within 15 s of sending.
- Meet's caption box never shows: every health event has `caption_box_shown` false. Meet's
  captions button may read on (`captions_on` true).
- At least 3 captions arrive after `recording_resumed`.
- macOS holds off idle sleep while the call records, and not after a stop or after the call.
- The stop ends the helper and closes the segment, and the app logs `recording_stopped`.
- No caption lands between `recording_stopped` and `recording_resumed`.
- After the call, `audio.json` lists at least 2 segments, all closed, and each segment's
  `NNN-mic.wav` and `NNN-remote.wav` are not empty.
- The note holds the chat message.

On a failure it prints the Meet tab's errors and mc-meet's stored sessions, and saves
`host-failure.png` and `guest-failure.png` in the work folder. Step times print in IST.

## Known traps

[driving-meet.md](driving-meet.md) lists the failures seen while driving Meet from scripts, with fixes.
