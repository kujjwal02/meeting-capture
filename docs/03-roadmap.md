# Roadmap

Each phase ends with a check on real calls. Estimates are evenings of about 3 hours.

| # | Phase | Delivers | Done when | Evenings |
|---|---|---|---|---|
| 0 | Skeleton | Workspace with crate stubs and a tray-icon Tauri app, pinned toolchain, rustfmt, clippy workspace lints, cargo-deny, cargo-shear, taplo, typos, biome, rumdl, lefthook hooks, `just check` | `just check` passes on the Mac and the PC; a misformatted file blocks a commit; a GPL crate fails `cargo deny check` | 2 |
| 1 | Tray app and captions pipeline | Tauri tray app with autostart and no Dock icon, caption receiver, job store, Google sign-in for work and personal calendars, calendar match, LLM extraction, vault writer, `mc` CLI | 5 real Meet calls write correct notes with no manual step; the tray lists them; a calendar sign-in still works after 8 days | 6 |
| 2 | Call detection | Start and end events, tray state | Meet, Teams, Zoom and a Slack huddle each log start and end within 5 s, on both OSes | 2 |
| 3 | Mac capture | Tap and mic stems, recovery of orphan WAVs at launch | 3 calls, one with AirPods connected mid-call, no gap over 1 s, app under 5% CPU | 5 |
| 4 | Local STT | Parakeet, VAD, remote diarization, caption-name merge | A 1 h call transcribes in under 10 min on the M2; names match captions on 3 Meet calls | 4 |
| 5 | Windows capture | WASAPI loopback, mic, Windows mic detection | Phase 3 checks pass on the PC | 4 |
| 6 | Extra sources | Soniox routing, Meet REST API, dropped audio files | A Hinglish call gets a Soniox transcript; the Meet API fills a missed call | 3 |
| 7 | Recording | OBS start and stop, audio archive, Drive folder | Video and audio for 3 calls land in the Drive folder, none in the vault | 2 |
| 8 | Hardening | Tests, GitHub Actions CI on macOS and Windows, model download with checksum, updater, commitment review | Two weeks without a lost meeting | 4 |

Phase 1 is useful on its own: with TranscripTonic it covers most Meet calls.

## Phase 0 detail

- `rust-toolchain.toml` pins Rust 1.98.1 with `clippy` and `rustfmt`.
- `[workspace.lints]` sets clippy `pedantic` and `unwrap_used` to warn and forbids `unsafe_code`
  outside the capture crates. `just check` passes `-D warnings`.
- `cargo-deny` allows MIT, Apache-2.0, BSD, ISC, Zlib, Unicode and MPL-2.0 licences, checks
  RustSec advisories, and allows crates.io only.
- `lefthook` runs fmt, taplo, typos and biome before each commit, and clippy before each
  push.
- `just check` runs every check locally. No CI and no tests until phase 8.
- `.gitattributes` sets `* text=auto eol=lf`, so Windows checkouts match rustfmt output.

## Phase 1 detail

- The app runs in the background from login, tray only (`ActivationPolicy::Accessory` on macOS).
  Plugins: `autostart`, `single-instance`, `notification`, `opener`.
- The axum receiver listens on `127.0.0.1:47615` inside the app and saves the raw TranscripTonic
  payload before parsing. TranscripTonic sends the "advanced" webhook body.
- The tray menu shows the listener state, each signed-in calendar account, and the last 5 meetings
  with their job state. It can retry failed jobs and open a note.
- The settings window is plain HTML: Google accounts, LLM provider, its API key, vault path, port.
- LLM provider is a closed choice: `anthropic` (default, Claude Haiku 5.5) or `openai-compatible`
  (base URL and model). The endpoint must support JSON-schema structured output; a reply that
  fails to parse or validate marks the job `Failed` and names the provider.
- Job states used: `Captured`, `Matched`, `Extracted`, `Written`, `Failed`.
- `mc reprocess <id>` and `mc ingest <file>` reuse the pipeline crates without Tauri.
- Logs are JSON lines from `tracing`, one span per job step with `meeting_id`, field names from the
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
- One refresh token per account in `keyring`.
- Calendar match queries both accounts and picks the event with the largest time overlap. On a tie,
  it prefers an event with a Meet link, then the akaiketech.com account.
- If no account matches or a token is revoked, the note is still written with the TranscripTonic
  title and no attendees, and the tray shows "sign in again".

### Build order

1. Webhook probe against a stub, transcript types.
2. Job store, vault writer, `mc reprocess` on a fixture.
3. Tray app grown from the phase 0 stub: receiver inside, autostart, single instance, self-signed build.
4. Google sign-in for both accounts, calendar match.
5. LLM extraction, fallback note, "note written" notification.
6. 5 real Meet calls.

## Open questions

- TranscripTonic v3.4.3 should post to `http://127.0.0.1`: its manifest allows `*://*/*` as an
  optional host permission, and its service worker sends the POST. Chrome's Local Network Access
  rules for extensions are untested. If the live test fails, phase 1 adds the Downloads watcher.
- Whether the akaiketech.com Workspace admin allows unverified third-party apps to read calendars.
  If not, add a second OAuth client with an "Internal" consent screen in the akaiketech.com org.
- Anarlog's docs say mic detection is macOS and Linux only, but `crates/detect/src/mic/windows.rs`
  exists. Test it in phase 2.
- Parakeet int8 speed on the M2 CPU. The only CPU figure is 13.7x real time on a Ryzen 4750U.
- Meet caption quality on Indian English and Hinglish.
- Haiku 5.5 against GLM-5.3-Flash on five real transcripts.
- Recording consent: tray shows recording state; a per-client "never record" list.

## Risks

| Risk | Mitigation |
|---|---|
| The global tap records notification sounds and music | VAD drops most of it; move to a per-process tap later |
| On speakers, remote voices also land in the mic stem | Drop mic segments that match an overlapping remote segment; Anarlog's `aec` crate later |
| A macOS update changes tap permissions | Check the tap at launch and warn in the tray |
| Parakeet on the Windows CPU is slow | Run STT when idle, or send Windows stems to Soniox |
| Vendored Anarlog code drifts from upstream | Record the upstream commit per vendored crate; diff monthly |
