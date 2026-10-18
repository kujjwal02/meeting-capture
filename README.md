# Meeting Capture

Bot-free meeting notes. A tray app turns the calls you take into Markdown notes in your Obsidian
vault.

[![Latest release](https://img.shields.io/github/v/release/kujjwal02/meeting-capture)](https://github.com/kujjwal02/meeting-capture/releases)
[![Check](https://github.com/kujjwal02/meeting-capture/actions/workflows/check.yml/badge.svg)](https://github.com/kujjwal02/meeting-capture/actions/workflows/check.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
![Platform: macOS](https://img.shields.io/badge/platform-macOS%20(Apple%20silicon)-lightgrey)

> [!NOTE]
> v0.1.0 is an early release for Apple silicon Macs. It turns Google Meet captions and Slack huddle
> captions into notes. Call detection, audio capture, local speech-to-text and the Windows build are
> on the [roadmap](docs/03-roadmap.md). The app is built for one person's machines, so expect rough
> edges.

## Why

Meeting bots join the call as a participant. They break when the browser, the lobby or the server
fails, and Google Meet now flags them as risky. Meeting Capture runs on the laptop you take the call
on, so no bot joins and nobody has to admit one.

## How it works

1. During a Google Meet call, the [TranscripTonic](https://github.com/vivek-nexus/transcriptonic)
   Chrome extension records the captions with real speaker names. When the call ends, it posts them
   to the app on `127.0.0.1`.
2. During a Slack huddle, the app reads Slack's live captions through the macOS Accessibility API.
3. The app matches the call to a Google Calendar event, for the title and attendees.
4. An LLM extracts the summary, decisions, action items, open questions and your commitments. Each
   item links to the transcript line it came from.
5. The app writes `<vault>/areas/meetings/YYYY/MM/YYYY-MM-DD-HHMM-<title-slug>.md`. If you edit
   the note, a later rerun keeps your edits.

The tray menu shows whether the receiver is listening and the last 5 meetings with their state. You
can open a note or retry a failed meeting from there.

## Privacy

| Data | Where it goes |
| --- | --- |
| Captions and transcripts | Stay on your Mac, in the job store and the note. |
| Transcript text for extraction | Sent to the LLM provider you choose. With no provider, notes hold the transcript only. |
| Calendar events | Read from Google Calendar with a read-only scope, through your own OAuth client. |
| Settings, API keys, OAuth tokens | Stored in plain text in `settings.toml` in the app's config folder. |
| Audio | v0.1.0 records no audio. |

The app sends no telemetry. Logs are written locally to `logs/app.log` in the app data folder.

## Install

Requirements: a Mac with Apple silicon, Google Chrome with TranscripTonic for Meet calls, and Slack
for huddles.

```sh
brew trust --tap kujjwal02/tap
brew install --cask kujjwal02/tap/meeting-capture
```

Or, without Homebrew:

```sh
curl -fsSL https://raw.githubusercontent.com/kujjwal02/meeting-capture/main/scripts/install.sh | sh
```

The app is self-signed and not notarized. Both commands install it without the "Apple could not
verify" prompt that a browser download gets.

The [install guide](docs/08-install.md) covers the DMG by hand, first-time setup, upgrades and
removal.

## Configure

Open Settings from the tray menu. It writes `settings.toml` in
`~/Library/Application Support/com.ujjwalkumar.meetingcapture`.

| Setting | Default | Purpose |
| --- | --- | --- |
| Google accounts | none | Calendar match. Needs your own OAuth client: see [Google setup](docs/06-google-setup.md). |
| LLM provider | none | `claude-code` (your signed-in Claude plan), `anthropic`, or any `openai-compatible` endpoint with JSON-schema output. See [LLM extraction](docs/07-llm-extraction.md). |
| `vault_dir` | `~/knowledge` | Where notes are written. |
| `my_name` | none | Replaces TranscripTonic's "You" in transcripts. |
| `receiver_port` | `47615` | Port TranscripTonic posts to: `http://127.0.0.1:47615/transcriptonic`. |
| `time_zone` | device zone | IANA name such as `Asia/Kolkata`, used in note names, dates and times. |

To replace the built-in note template, add `note.md.j2` to the same folder. Quit and reopen the app
after a change.

## Command-line tool

`mc` runs the same pipeline without the tray app. Install it with
`cargo install --path crates/cli`.

| Command | Does |
| --- | --- |
| `mc list` | Shows recent meetings and their state. |
| `mc ingest <payload.json>` | Adds a saved TranscripTonic body. |
| `mc retry <id>` | Resumes a failed meeting. |
| `mc reprocess <id>` | Reruns every step. It keeps a note you edited; `--discard-edits` overwrites it. |

## Build from source

Install the tools once:

```sh
brew install just lefthook taplo typos-cli biome rumdl cargo-deny cargo-shear openssl@3
cargo install tauri-cli --version "^2" --locked
just hooks
```

`rust-toolchain.toml` pins Rust, and rustup installs it on the first build.

| Command | Does |
| --- | --- |
| `just dev` | Runs the tray app in dev mode. |
| `just check` | Runs every format, lint, licence and advisory check. The Check workflow runs it on each push. |
| `just fmt` | Rewrites files to the formatters' output. |
| `just signing-cert` | Adds the self-signed code-signing identity to the login keychain, once. macOS keeps the app's permissions across updates only with a stable identity. |
| `just release` | Runs `just check`, then builds the signed DMG, the updater package and `latest.json` in `target/dist`. |

To release, set `version` in `Cargo.toml`, merge to `main`, then push a tag such as `v0.2.0`. The
Release workflow runs the checks, builds and signs the app, publishes the GitHub release, and
updates the [Homebrew cask](https://github.com/kujjwal02/homebrew-tap). Installed apps pick up the
release within 6 hours.

To keep a dev run away from your real job store, set `MEETING_CAPTURE_HOME` to a scratch folder. The
app then reads `config/settings.toml` and writes `data/` inside it.

## Documentation

- [Overview](docs/01-overview.md): problem, goals, non-goals, budget
- [Architecture](docs/02-architecture.md): components, design rules, transcript format
- [Roadmap](docs/03-roadmap.md): phases, open questions, risks
- [Libraries](docs/04-libraries.md): crates per job, what was ruled out
- [Checklist](docs/05-checklist.md): done and open items per phase
- [Google setup](docs/06-google-setup.md): the OAuth client for calendar sign-in
- [LLM extraction](docs/07-llm-extraction.md): prompt, quote check, providers, retries, note sections
- [Install](docs/08-install.md): install, upgrade, uninstall

## Contributing

This is a personal project, built for one person's calls. Bug reports and pull requests are welcome.
Large features may not fit the [roadmap](docs/03-roadmap.md), so open an issue first. Run
`just check` before you push.

## Acknowledgements

[TranscripTonic](https://github.com/vivek-nexus/transcriptonic) records the Google Meet captions.
The app is built with [Tauri](https://tauri.app) and [rig](https://github.com/0xPlaygrounds/rig).

## License

[MIT](LICENSE) © 2026 Ujjwal Kumar
