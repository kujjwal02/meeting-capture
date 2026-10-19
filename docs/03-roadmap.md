# Roadmap

Each phase ends with a check on real calls. Estimates are evenings of about 3 hours.

| # | Phase | Delivers | Done when | Evenings |
|---|---|---|---|---|
| 0 | Skeleton | Workspace with crate stubs and a tray-icon Tauri app, pinned toolchain, rustfmt, clippy workspace lints, cargo-deny, cargo-shear, taplo, typos, biome, rumdl, lefthook hooks, `just check` | `just check` passes on the Mac and the PC; a misformatted file blocks a commit; a GPL crate fails `cargo deny check` | 2 |
| 1 | Tray app and captions pipeline | Tauri tray app with autostart and no Dock icon, caption receiver, Slack huddle captions on the Mac, job store, Google sign-in for work and personal calendars, calendar match, LLM extraction, vault writer, `mc` CLI | 5 real Meet calls and 3 Slack huddles write correct notes with no manual step; the tray lists them; a calendar sign-in still works after 8 days | 6 |
| 1b | mc-meet extension | Our own Meet extension: captions, chat and roster from Meet's data channels with a DOM fallback, native messaging to the app, one event log per call, attendees who joined. Replaces TranscripTonic and the webhook receiver. [Spec](09-meet-extension.md) | 3 Meet calls, one with a second Meet tab, one browser restart and one app quit mid-call, each give one note whose attendees joined the call | 8 |
| 2 | Call detection | Start and end events, tray state. The extension's `call_started` and `call_ended` cover Meet in the browser | Meet, Teams, Zoom and a Slack huddle each log start and end within 5 s, on both OSes | 2 |
| 3 | Mac capture | Tap and mic stems, recovery of orphan WAVs at launch | 3 calls, one with AirPods connected mid-call, no gap over 1 s, app under 5% CPU | 5 |
| 4 | Local STT | Parakeet, VAD, remote diarization, caption-name merge | A 1 h call transcribes in under 10 min on the M2; names match captions on 3 Meet calls | 4 |
| 5 | Windows capture | WASAPI loopback, mic, Windows mic detection | Phase 3 checks pass on the PC | 4 |
| 6 | Extra sources | Soniox routing, dropped audio files, calendar from a secret iCal address | A Hinglish call gets a Soniox transcript; an iCal address matches a call with no Google sign-in | 3 |
| 7 | Recording | OBS start and stop, audio archive, Drive folder | Video and audio for 3 calls land in the Drive folder, none in the vault | 2 |
| 8 | Hardening | Tests, model download with checksum, commitment review | Two weeks without a lost meeting | 4 |

Phase 1 is useful on its own: with TranscripTonic it covers most Meet calls. Phase 1b replaces
TranscripTonic, which split a call into two notes when a second Meet tab opened and listed
invitees as attendees.

The Meet REST API is out. On Business Starter it gives no transcripts or recordings, and the
extension already sees every call the user joins.

Progress is tracked in the [checklist](05-checklist.md).

## Phase 0 detail

- `rust-toolchain.toml` pins Rust 1.98.1 with `clippy` and `rustfmt`.
- `[workspace.lints]` sets clippy `pedantic` and `unwrap_used` to warn and forbids `unsafe_code`
  outside the capture crates. `just check` passes `-D warnings`.
- `cargo-deny` allows MIT, Apache-2.0, BSD, ISC, Zlib, Unicode and MPL-2.0 licences, checks
  RustSec advisories, and allows crates.io only.
- `lefthook` runs fmt, taplo, typos and biome before each commit, and clippy before each
  push.
- `just check` runs every check. The Check workflow runs it on macOS for each push, and clippy on
  Windows. No tests until phase 8.
- `.gitattributes` sets `* text=auto eol=lf`, so Windows checkouts match rustfmt output.

## Phase 1 detail

- The app runs in the background, tray only (`ActivationPolicy::Accessory` on macOS), and starts
  at login once "Start at login" is on. Plugins: `autostart`, `single-instance`, `opener`, and
  `notification` from build step 5.
- The axum receiver listens on `127.0.0.1:47615` inside the app and saves the raw TranscripTonic
  payload before parsing. TranscripTonic sends the "advanced" webhook body.
- TranscripTonic v3.4.3 reaches `http://127.0.0.1:47615/transcriptonic` from Chrome (live test on
  2026-10-17), so phase 1 needs no Downloads watcher.
- TranscripTonic names the local user "You". The pipeline replaces it with the user's name from
  settings.
- A Meet call with no calendar event arrives titled "Meet – abc-defg-hij". The parser turns the
  code into `meeting_url` and drops the title.
- The tray menu shows the listener state, each signed-in calendar account, and the last 5 meetings
  with their job state. It can retry failed jobs and open a note.
- The settings window is plain HTML: Google accounts and OAuth client, LLM provider, its API key,
  vault path, port, your name, time zone. It writes `settings.toml`, secrets included.
- LLM provider is a closed choice: `claude-code` (the signed-in Claude plan, Sonnet by default),
  `anthropic` (Claude Haiku 5.5 by default) or `openai-compatible` (base URL and model). With no
  provider, extraction is off and notes carry the transcript only. The endpoint must support
  JSON-schema structured output. A bad reply gets 2 repair turns, then the attempt fails, names the
  provider and model, and is retried. [LLM extraction](07-llm-extraction.md) has the detail.
- Job states used: `Captured`, `Matched`, `Extracted`, `Written`, `ExtractionPending`, `Failed`.
- `mc reprocess <id>` and `mc ingest <file>` reuse the pipeline crates without Tauri.
- Logs are JSON lines from `tracing`, one span per job step with `meeting.id`, field names from the
  OpenTelemetry semantic conventions. Adding an OTLP exporter later is one `tracing-opentelemetry`
  layer.

### Google sign-in

- "Add Google account" in settings, once for akaiketech.com and once for the personal account.
- Authorization code flow with PKCE and a loopback redirect, in the default browser. Google blocks
  sign-in inside embedded webviews.
- Scopes: `calendar.events.readonly`, `openid`, `email`.
- One OAuth client of type "Desktop app" in a personal Google Cloud project, consent screen
  "External" and published "In production". A consent screen in "Testing" expires refresh tokens
  after 7 days. Unverified apps show a warning at consent, which is fine for personal use.
- The client ID, the client secret and one refresh token per account go in the settings file. The
  repo is public, so none of them is in the code.
- Calendar match reads each account's primary calendar. An event with the transcript's Meet code
  wins. Else the largest time overlap wins; on a tie, an event with a Meet link, then the account
  listed first. Sign in with akaiketech.com first, so it wins ties.
- All-day events, declined events, focus time, out of office and working location never match.
- A network failure fails the run, which keeps the calendar title once retried.
- If no account matches or a token is revoked, the note is still written with the TranscripTonic
  title and no attendees, and the tray shows "sign in again".

### Build order

1. Webhook probe against a stub, transcript types.
2. Job store, vault writer, `mc reprocess` on a fixture.
3. Tray app grown from the phase 0 stub: receiver inside, autostart, single instance, self-signed build.
4. Google sign-in for both accounts, calendar match.
5. LLM extraction, fallback note, "note written" notification.
6. Slack huddle captions, read from the Slack desktop app through the accessibility API.
7. 5 real Meet calls and 3 Slack huddles.

### Slack huddle captions

- The `huddle-captions` crate reads the huddle window of the Slack desktop app on the Mac: caption
  rows with speaker names, participant tiles and the caption language. Research and live tests:
  `~/knowledge/areas/infrastructure/meeting-recording/local-capture/slack-huddles/`.
- It needs the Accessibility permission. Slack's own preference "Automatically turn on captions
  when you're in a huddle" turns captions on; the app then presses the huddle's Captions tab. The
  app cannot turn captions on itself: that menu opens only on a real mouse hover.
- Each huddle is a log in the app data folder, one JSON line per event, written before any parsing.
  The log is the meeting's input, like a TranscripTonic payload.
- Huddles are never matched to calendar events. They keep the channel title and the people Slack
  showed.
- Markers live in layout profiles, TOML files per Slack version. A file in the app's
  `slack-layouts` config folder overrides the built-in one without a rebuild.
- The watcher never fails silently: degraded, captions-off and broken states each show in the tray
  and send one notification per huddle.
- A huddle under `min_huddle_seconds` (default 120) or with fewer than 3 caption lines writes no
  note. `mc ingest --kind slack-huddle-log` writes one from its log.

## Open questions

- Whether the akaiketech.com Workspace admin allows unverified third-party apps to read calendars.
  If not, add a second OAuth client with an "Internal" consent screen in the akaiketech.com org.
- Anarlog's docs say mic detection is macOS and Linux only, but `crates/detect/src/mic/windows.rs`
  exists. Test it in phase 2.
- Parakeet int8 speed on the M2 CPU. The only CPU figure is 13.7x real time on a Ryzen 4750U.
- Meet caption quality on Indian English and Hinglish.
- Haiku 5.5, Sonnet through Claude Code and GLM-5.3-Flash on five real transcripts.
- Codex as a fourth provider, through `codex exec`.
- Recording consent: tray shows recording state; a per-client "never record" list.

## Risks

| Risk | Mitigation |
|---|---|
| The global tap records notification sounds and music | VAD drops most of it; move to a per-process tap later |
| On speakers, remote voices also land in the mic stem | Drop mic segments that match an overlapping remote segment; Anarlog's `aec` crate later |
| A macOS update changes tap permissions | Check the tap at launch and warn in the tray |
| Parakeet on the Windows CPU is slow | Run STT when idle, or send Windows stems to Soniox |
| Vendored Anarlog code drifts from upstream | Record the upstream commit per vendored crate; diff monthly |
