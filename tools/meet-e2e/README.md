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
`MEET_E2E_DIR` to move the work folder from `~/.cache/meeting-capture-e2e`.

## What it does

1. Copies the dev binary into the work folder and starts it with its own home and vault.
2. Writes a `com.meetingcapture.app` host manifest into the host browser's data folder. Chrome
   reads it before the global one, so only the host browser starts the dev app.
3. Loads `extension/` into the host browser and gives the host a fake microphone that reads a
   `say` recording.
4. The host opens a new meeting. The guest asks to join, muted, and the host admits it from the
   toast or the People panel. The guest sends a chat message.
5. Both leave. The run waits for `call_ended` in the call log and for the note in the vault.

The dev app rewrites the global host manifests of Chrome, Chromium, Brave and Edge at start. The
run restores them and the host browser's manifest at the end, also after a failure. While it runs,
mc-meet in your own browsers starts the dev binary without the dev home.

## What it checks

- The log has one session, starting with `call_started` and ending with `call_ended`.
- The log has at least 3 captions, all from the data channel.
- A roster lists both people.
- The chat time is within 15 s of sending.
- The last health event has captions on.
- The note holds the chat message.

On a failure it prints the Meet tab's errors and mc-meet's stored sessions, and saves
`host-failure.png` and `guest-failure.png` in the work folder. Step times print in IST.
