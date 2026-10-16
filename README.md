# meeting-capture

Bot-free meeting transcripts for Mac and Windows. A Rust tray app detects calls, records the mic and
system audio, receives Google Meet captions, transcribes locally after the call, and writes a meeting
note into the Obsidian vault.

Status: phase 0. A workspace skeleton and a tray icon with a Quit item.

- [Overview](docs/01-overview.md): problem, goals, non-goals, budget
- [Architecture](docs/02-architecture.md): components, design rules, transcript format
- [Roadmap](docs/03-roadmap.md): phases, open questions, risks
- [Libraries](docs/04-libraries.md): crates per job, what was ruled out, what we write

## Development

Install the tools once (Mac):

```sh
brew install just lefthook taplo typos-cli biome rumdl cargo-deny cargo-shear
cargo install tauri-cli --version "^2" --locked
just hooks
```

`rust-toolchain.toml` pins Rust; rustup installs it on first build.

- `just check`: every format, lint, licence and advisory check. Run it before pushing.
- `just fmt`: rewrite files to the formatters' output.
- `just dev`: run the tray app.
- `just signing-cert`: once per Mac, add a self-signed code-signing identity to the login
  keychain, so macOS keeps the app's permissions across rebuilds. Needs `brew install openssl@3`.
- `just bundle`: build the signed `Meeting Capture.app` in `target/release/bundle/macos`.
- `cargo run -p cli -- list`: show recent meetings. `mc ingest <payload.json>` adds a saved
  TranscripTonic body, `mc reprocess <id>` reruns every step, and `mc retry <id>` resumes a failed
  meeting. A note you edited is kept; `mc reprocess <id> --discard-edits` overwrites it.

The tray menu shows whether the receiver is listening and the last 5 meetings with their state. Each
meeting can open its note or retry a failure, and "Retry failed" retries them all. "Start at login"
adds a LaunchAgent; dev builds show it disabled. "Show log file" selects the app's JSON log in
Finder, `logs/app.log` in the app data folder (`~/Library/Application Support/
com.ujjwalkumar.meetingcapture` on the Mac).

Settings live in `settings.toml` in the app config folder (`~/Library/Application Support/
com.ujjwalkumar.meetingcapture` on the Mac). Fields: `vault_dir` (default `~/knowledge`), `my_name`
(replaces TranscripTonic's "You"), `receiver_port` (default 47615), and `time_zone` (an IANA name
such as `Asia/Kolkata`; default the device's zone, used for note names, note dates and shown times).
An optional `note.md.j2` in the same folder replaces the built-in note template. "Open settings
file" in the tray creates the file with every field commented out. Quit and reopen the app after a
change.

Set `MEETING_CAPTURE_HOME` to a scratch folder to keep a dev run away from the real job store. It
then reads `config/settings.toml` and writes `data/` inside that folder.
