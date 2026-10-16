# Checklist

Every phase and build step from the [roadmap](03-roadmap.md), with what each one delivers.
A ticked box is done and committed. A phase is done when its "done when" boxes are ticked.

## Phase 0: skeleton

- [x] Cargo workspace with stub crates: `app`, `caption-receiver`, `cli`, `jobs`, `pipeline`,
  `sources`.
- [x] Tauri 2 tray app: tray icon with a Quit item, no Dock icon.
- [x] `rust-toolchain.toml` pins Rust 1.98.1 with `clippy` and `rustfmt`.
- [x] `[workspace.lints]`: clippy `pedantic` and `unwrap_used` warn, `unsafe_code` forbidden outside
  the capture crates.
- [x] `cargo-deny`: permissive licences only, RustSec advisories, crates.io only, Mac and Windows
  targets.
- [x] cargo-shear, taplo, typos, biome and rumdl.
- [x] lefthook: format checks before each commit, clippy before each push.
- [x] `just check` runs every check with `-D warnings`.
- [x] `.gitattributes` sets `* text=auto eol=lf`.
- [x] Docs: overview, architecture, roadmap, libraries. MIT licence.

Done when:

- [x] `just check` passes on the Mac.
- [ ] `just check` passes on the PC.
- [ ] A misformatted file blocks a commit.
- [ ] A GPL crate fails `cargo deny check`.

## Phase 1: tray app and captions pipeline

### Step 1: webhook receiver and transcript types

- [x] Common `Transcript` type in `sources`: source, start and end, platform, meeting URL, title,
  speaker naming, segments, chat messages.
- [x] TranscripTonic v3.4.3 "advanced" webhook body parsed into a `Transcript`. Parse errors name
  the field.
- [x] Caption blocks sorted by time; a missing `transcript` or `chatMessages` key reads as empty.
- [x] "Meet – abc-defg-hij" titles become `meeting_url`, on Meet calls only.
- [x] axum receiver on `127.0.0.1:47615/transcriptonic`, 4 MB body limit.
- [x] Sender checks: TranscripTonic's extension Origin, the receiver's own Host, JSON body. Else
  403.
- [x] Raw payload saved and synced to disk under a unique name before the 200 reply.
- [x] Live test: TranscripTonic in Chrome reaches the receiver (2026-10-17). No Downloads watcher
  needed.
- [x] Review by a subagent; findings fixed.

### Step 2: job store, vault writer, `mc`

- [x] SQLite job store in `jobs` with migrations. States: `Captured`, `Matched`, `Extracted`,
  `Written`, `Failed`.
- [x] A re-post of the same meeting updates its row and rewrites its note.
- [x] Run lease and input revision: one run per meeting across the tray app and `mc`. A run
  restarts on a newer capture and stops when it loses its lease.
- [x] Settings file (`settings.toml`): vault folder, your name, receiver port. Errors name the
  field.
- [x] Note template (`note.md.j2`), overridable from the config folder. Frontmatter follows the
  vault's meeting note rules.
- [x] Note names `YYYY/MM/yyyy-mm-dd-hhmm-<title-slug>.md`, with the meeting id added on a clash.
- [x] Atomic note write: staged outside the vault, renamed in, folder synced. Works for a vault on
  another disk.
- [x] Transcript text escaped, so it cannot add tags, links, headings or comments to a note.
- [x] Edit guard: a note the user edited is never overwritten (SHA-256 of each write).
- [x] "You" replaced by your name from settings.
- [x] `mc ingest`, `mc reprocess [--discard-edits]`, `mc retry`, `mc list`. Times shown in IST.
- [x] JSON logs from `tracing` with OpenTelemetry field names.
- [x] Review by two subagents; findings fixed.

### Step 3: tray app

- [ ] Receiver runs inside the app; each payload becomes a job and runs the pipeline.
- [ ] The receiver replies 200 only after the job is recorded.
- [ ] At launch, run every job left in `Captured` or with an expired lease.
- [ ] Autostart at login, single instance, tray only (`ActivationPolicy::Accessory`).
- [ ] Tray menu: listener state, the last 5 meetings with job state, retry a failed job, open a
  note.
- [ ] Settings window (plain HTML): vault path, port, your name.
- [ ] Self-signed build.

### Step 4: Google sign-in and calendar match

- [ ] OAuth client of type "Desktop app", consent screen "External" and "In production".
- [ ] "Add Google account": authorization code flow with PKCE and a loopback redirect in the
  default browser.
- [ ] Scopes `calendar.events.readonly`, `openid`, `email`.
- [ ] One refresh token per account in `keyring`; akaiketech.com and personal accounts.
- [ ] Calendar match across both accounts by the largest time overlap. On a tie, prefer a Meet
  link, then akaiketech.com.
- [ ] No match or a revoked token: the note keeps the TranscripTonic title, and the tray shows
  "sign in again".
- [ ] Tray shows each signed-in account.
- [ ] Settle whether the akaiketech.com admin allows unverified apps to read calendars.

### Step 5: LLM extraction

- [ ] Provider setup through rig: `anthropic` (default, Claude Haiku 5.5) or `openai-compatible`
  (base URL and model).
- [ ] Extraction with `prompt_typed` and a JSON schema. A reply that fails to validate marks the
  job `Failed` and names the provider.
- [ ] Lease renewed during the LLM call.
- [ ] Fallback note: transcript only, tagged `status/needs-extraction`, retried later.
- [ ] Note sections rewritten without losing the user's edits elsewhere in the note.
- [ ] Settings window: LLM provider and API key.
- [ ] "Note written" notification.
- [ ] Compare Haiku 5.5 against GLM-5.3-Flash on five real transcripts.

### Step 6: real calls

- [ ] 5 real Meet calls write correct notes with no manual step.
- [ ] The tray lists them.
- [ ] A calendar sign-in still works after 8 days.
- [ ] Windows build checked on the PC.

## Phase 2: call detection

- [ ] `detect` reports when an app takes or releases the mic, and which app.
- [ ] Start and end events reach the app; the tray shows call state.
- [ ] Test Anarlog's Windows mic detection (`crates/detect/src/mic/windows.rs`).

Done when:

- [ ] Meet, Teams, Zoom and a Slack huddle each log start and end within 5 s, on the Mac.
- [ ] The same on the PC.

## Phase 3: Mac capture

- [ ] `capture` with one `CallAudioSource` trait; Core Audio tap for remote audio, `cpal` for the
  mic.
- [ ] `stem-writer` writes `mic.wav` and `remote.wav`; the app connects it to `capture`.
- [ ] Orphan WAVs recovered at launch.
- [ ] Tap permission checked at launch, with a tray warning.

Done when:

- [ ] 3 calls recorded, one with AirPods connected mid-call, no gap over 1 s.
- [ ] The app stays under 5% CPU.

## Phase 4: local speech-to-text

- [ ] `stt`: VAD, Parakeet TDT 0.6B v3, diarization of the remote stem.
- [ ] Caption names merged onto diarized speakers.
- [ ] `sources` picks the best transcript per meeting.
- [ ] Measure Parakeet int8 speed on the M2 CPU.

Done when:

- [ ] A 1 h call transcribes in under 10 min on the M2.
- [ ] Names match captions on 3 Meet calls.

## Phase 5: Windows capture

- [ ] WASAPI process loopback for remote audio; `cpal` for the mic.
- [ ] Windows mic detection.

Done when:

- [ ] Phase 3 checks pass on the PC.

## Phase 6: extra sources

- [ ] Hinglish calls routed to Soniox.
- [ ] Meet REST API fills a call TranscripTonic missed.
- [ ] Dropped audio files transcribed.
- [ ] Check Meet caption quality on Indian English and Hinglish.

Done when:

- [ ] A Hinglish call gets a Soniox transcript.
- [ ] The Meet API fills a missed call.

## Phase 7: recording

- [ ] `obs-control` starts and stops OBS video recording with the call.
- [ ] Audio archive and a Drive folder for media. No media in the vault.
- [ ] Recording consent: the tray shows recording state; a per-client "never record" list.

Done when:

- [ ] Video and audio for 3 calls land in the Drive folder, none in the vault.

## Phase 8: hardening

- [ ] Tests.
- [ ] GitHub Actions CI on macOS and Windows.
- [ ] Model download with checksum.
- [ ] Updater.
- [ ] Commitment review.
- [ ] Monthly diff of vendored Anarlog crates against upstream.

Done when:

- [ ] Two weeks without a lost meeting.
