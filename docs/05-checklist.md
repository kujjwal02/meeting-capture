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
- [x] Note names, note dates and shown times use the `time_zone` setting, by default the
  device's. Stored times stay UTC.
- [x] `mc ingest`, `mc reprocess [--discard-edits]`, `mc retry`, `mc list`. Times shown in IST.
- [x] JSON logs from `tracing` with OpenTelemetry field names.
- [x] Review by two subagents; findings fixed.

### Step 3: tray app

- [x] Receiver runs inside the app; each payload becomes a job and runs the pipeline.
- [x] The receiver replies 200 only after the job is recorded.
- [x] Every unfinished meeting runs at launch and every 5 minutes, each on its own thread. A dead
  process's lease expires after 2 minutes; a running pipeline renews it every 30 s for up to
  15 minutes. The note write checks the lease first.
- [x] A panic in a run is recorded as the meeting's failure. Panics and setup errors go to the
  log.
- [x] Single instance, tray only (`ActivationPolicy::Accessory`, `LSUIElement`).
- [x] "Start at login" toggle in the tray (a LaunchAgent), release builds only.
- [x] Tray menu: listener state, the last 5 meetings with job state, open a note, retry one or
  all failed meetings, open the settings file, show the log file.
- [x] JSON log in `logs/` in the app data folder, one `app-<UTC date>.jsonl` per day.
- [x] Every tray "open" names its app: notes open in Obsidian (else the text editor), the
  settings file in the text editor, the log file selected in Finder. None depends on a file
  type's default app.
- [x] `just signing-cert` creates the self-signed "Meeting Capture" identity and writes it as a
  `.p12` file. `just release` and the Release workflow sign with it.
- [x] Create the signing identity.
- [ ] Install the v0.1.0 DMG from the GitHub release and take one Meet call with it.
- [x] The app checks GitHub releases every hour, downloads a newer version, and installs it from
  the tray item or at quit.
- [x] Check workflow: `just check` on macOS, clippy on Windows. Release workflow on a `v*` tag:
  build, sign, publish, update the Homebrew cask.
- [ ] Update an installed v0.2.0 to the next release through the tray item.
- [ ] Check the tray menu by hand: listener line, meetings, retry, open note, start at login.
- [ ] After a reboot the tray icon appears with no Dock icon and no window, and says
  "Listening".

### Step 4: Google sign-in and calendar match

- [x] Settings window (plain HTML) with the sign-in button, OAuth client fields and a client JSON
  loader, vault path, port, your name and time zone. It replaces "Open settings file".
- [x] OAuth client of type "Desktop app", consent screen "External" and "In production", in the
  `ujjwalk-homelab` project. Steps in `docs/06-google-setup.md`.
- [x] "Add Google account": authorization code flow with PKCE, a state check and a loopback
  redirect in the default browser. Gives up after 5 minutes.
- [x] Scopes `calendar.events.readonly`, `openid`, `email`.
- [x] Client secret and one refresh token per account in `settings.toml`; a dev run under
  `MEETING_CAPTURE_HOME` uses its own settings file.
- [x] Calendar match across every account: same Meet code, then the largest time overlap, then a
  Meet link, then the account listed first.
- [x] No match or a revoked token: the note keeps the TranscripTonic title, and the tray shows
  "sign in again". A network failure fails the run instead.
- [x] Tray shows each signed-in account. Accounts are checked at launch.
- [x] Review by a subagent; findings fixed.
- [ ] Sign in with akaiketech.com and the personal account, then match a real call.
- [ ] Settle whether the akaiketech.com admin allows unverified apps to read calendars.

### Step 5: LLM extraction

- [x] Providers: `claude-code` (signed-in plan, Sonnet by default), and through rig `anthropic`
  (Claude Haiku 5.5 by default) or `openai-compatible` (base URL and model). Off until one is
  chosen.
- [x] Extraction with `prompt_typed` and a JSON schema from the Rust type. Every item cites
  transcript lines; the quote check verdicts match the Python prototype on 26 recorded replies.
- [x] A bad reply gets 2 repair turns, then the attempt fails and names the provider and model.
- [x] Lease renewed during the LLM call (step 3 renews it for every run).
- [x] Timeouts: 3 minutes per API call; 5 minutes per Claude Code turn and 10 in total.
- [x] Fallback note: transcript only, tagged `status/needs-extraction`, retried after 5 minutes,
  30 minutes and 2 hours, then `Failed`.
- [x] Note sections between `%% mc:` markers, rewritten without losing the user's edits elsewhere
  in the note or their frontmatter keys and tags.
- [x] Settings window: LLM provider, model, base URL, API key in the settings file, "Check Claude Code".
- [x] A note-edited failure offers "Overwrite my edits" in the tray, so it can be cleared without
  `mc`.
- [x] "Note written" notification.
- [ ] Anthropic path run against the real API.
- [ ] Compare Haiku 5.5, Sonnet through Claude Code and GLM-5.3-Flash on five real transcripts.
- [x] Review by a subagent; findings fixed.

### Step 6: Slack huddle captions

- [x] `huddle-captions` crate: Mac adapter over the accessibility API, layout profiles as TOML with
  an override folder, three matching levels for the caption panel, overlay fallback, health states.
- [x] Huddle window found by Slack's "Child window toolbar" and the huddle content class. An
  unreadable child window counts as a broken huddle only while the main window shows "Leave huddle".
- [x] Captions tab pressed when the panel is hidden: once at the start, a retry after 10 s, and after
  the user hides the panel only once it has stayed hidden for 2 minutes.
- [x] Huddle log schema 1, one file per huddle, flushed per line; open logs get an `ended` line at
  launch and become meetings.
- [x] `sources::slack_huddle` parser: rows, overlay lines only while the panel was hidden, backlog
  rows spread before the first live row, attendees from tiles, the self tile as "me".
- [x] `InputKind::SlackHuddleLog`, `mc ingest --kind slack-huddle-log`, huddles never matched to the
  calendar.
- [x] Note fields: `slack_workspace`, `slack_channel`, `slack_attendees`, `duration_min`,
  `language`, `captions`, `slack_version`; tags `topic/slack-huddle` and
  `status/captions-degraded`.
- [x] Settings: "Read Slack huddle captions" and "Start captions automatically", applied from the
  next huddle; `min_huddle_seconds` in the file.
- [x] Tray line per huddle state, Accessibility prompt and a menu item to its settings pane, one
  notification per huddle and warning state.
- [x] Two live solo huddles on Slack 4.52.171: captions tab pressed 2 s after start, a Thread-tab
  visit kept, notes written from the logs.
- [ ] A huddle with two or more people: join and leave, both speakers' rows, DM titles.
- [x] Closing and reopening the huddle window mid-huddle keeps one log; lines said while it was
  closed come from the reopened panel.
- [ ] A Thread visit over 2 minutes, and a busy Slack.
- [ ] A 45-minute huddle: does the panel drop or renumber rows?
- [ ] "Save Slack layout dump" and `mc slack-layout probe`.
- [ ] `previous_huddle` link, `channel_clients` map, join and leave times in the note.
- [ ] Purge huddle logs after 90 days.
- [ ] Resume a log when the app restarts during a huddle (today it ends the log and starts another).
- [ ] CPU of Slack plus the watcher against a captions-off baseline over 10 minutes.
- [ ] Windows build checked on the PC (the crate builds a no-op there).

### Step 7: real calls

- [ ] 5 real Meet calls and 3 Slack huddles write correct notes with no manual step.
- [ ] The tray lists them.
- [ ] A calendar sign-in still works after 8 days.
- [ ] Windows build checked on the PC.

## Phase 1b: mc-meet extension

[Spec](09-meet-extension.md).

- [x] `mc merge <kept-id> <dropped-id>` joins two TranscripTonic meetings of one call. Meetings 32
  and 33 of 2026-10-19 merged.
- [x] `attendees` lists only people who joined: the roster, else speakers and chat senders plus
  you. Calendar invitees go to `invited`, with their response kept in the stored match.
- [x] Host mode in the app binary relays native messaging frames to the app socket, and starts
  the app when it is not running.
- [x] The app writes the host manifest for each Chromium browser on every start.
- [x] Browser call events: one JSONL log per session, acked, ended on `call_ended` or 5 minutes
  without a port, closed after a crash. `InputKind::BrowserCallLog` and its parser.
- [x] mc-meet extension: data channel decoder, DOM fallback, session per tab, acked resend, no
  telemetry.
- [x] Tray line: extension connected, and one status line per open call.
- [x] Call status board for Meet calls and huddles: waiting, recording, problem with a reason,
  ended. Tray icon colour, tooltip, "Recent calls", and deduplicated notifications.
- [x] mc-meet reports Meet refusing captions as `captions_unavailable`, and never reports
  `captions_on` while Meet refuses them.
- [x] mc-meet shows each tab's call status on its toolbar icon and in a popup.
- [x] Debug material per call: failing frames with bytes, the 2-minute lead-up, page snapshots.
  "Save debug report" in the tray and the popup. `just replay-frames`. Deleted after 14 days.
- [x] A rejoin of the same meeting within 10 minutes goes into the same log and note.
- [x] Captions turn on when the call starts, so the first words are kept.
- [x] `[meet] hide_captions`, default on: captions go off after the first channel caption and
  mc-meet keeps recording them. The page hook blocks Meet's `close()` on its caption channel.
- [ ] The DOM reader splits a long single-speaker turn into caption lines. A 1200-character
  monologue now gives one line and no note.
- [x] TranscripTonic uninstalled; caption receiver and its docs removed.

Done when:

- [x] 3 Meet calls, one with a second Meet tab, one browser restart and one app quit mid-call,
  each give one note whose attendees joined the call.
- [ ] A Meet call where Meet refuses captions shows amber in the tray and on the mc-meet icon,
  and a "Not recording" notification names the reason within 1 minute.

## Phase 2: call detection

- [x] `calls` turns mc-meet and huddle events into one call stream and logs each event.
- [x] Each call ended at launch logs `call.ended` with reason `app_restarted`.

Done when:

- [x] Meet and a Slack huddle each log start within 5 s on the Mac. Meet logs its end within 5 s;
  a huddle within 6 s, with the end time in its log set to when its window was last seen.
- [x] A Meet rejoin within 10 minutes logs `call.resumed` with the same `call.id`.
- [x] An app killed mid-call logs `call.ended` with `app_restarted`, then a fresh `call.started`.
- [ ] Meet logs start and end within 5 s on the PC.

## Phase 3: Mac capture

- [x] Spike: tap format, helper prompt attribution, CPU with the tap open.
- [x] Spike: tap across a switch to AirPods, AirPods HFP rate.
- [x] Usage strings in both plists; minimum macOS 14.4; the audio-input entitlement.
- [x] `capture` with one `CallAudioSource` type; global tap for remote audio, `cpal` for the mic.
- [x] `stem-writer` writes numbered `NNN-mic.wav` and `NNN-remote.wav` segments and `audio.json`.
- [x] `mc-capture` helper, supervised by the app with 1, 2 and 4 s restarts.
- [ ] Built-in mic when the default input is Bluetooth; new mic stream opens before the old closes.
- [ ] Device listeners; the tap rebuilds only on evidence, at most 4 times per episode.
- [ ] Sleep closes the segment; wake opens the call's next segment.
- [x] Orphan WAVs repaired at launch, and at each segment start under the folder's
  `audio.lock`. A second run finds nothing. A folder whose helper records is skipped.
- [ ] System-audio and mic permission checked at each segment start and after a failure, with a
  tray warning.
- [x] Tray recording dot and "Stop recording this call".

Done when:

- [ ] 3 calls recorded, one with AirPods connected mid-call. In `audio.json`, no gap is over 1 s,
  except a `device_switch` gap of up to 3 s paired with a `device_changed` report, or up to 7 s
  paired with a `tap_stalled` one, one per change.
- [ ] The app and the helper together stay under 5% CPU in `top -l 0 -s 10`.
- [x] A `kill -9` of the app and of the helper mid-call each leave stems that play. Checked on
  fake mc-meet calls with the built-in mic and the tap. Each segment lost at most 0.33 s of its
  wall time. Each played in `afinfo`. The call's next segment took the next number.

Unresolved live tests. The code for each is committed and passed injected-event or fake mc-meet
runs. None has run on a real call yet.

- [x] A live `just meet-e2e` run records a Meet call with no click. A Workspace account hosted
  and a personal account joined. Stop and resume through the popup gave two closed segments with
  nonzero stems, and macOS held off idle sleep only while recording.
- [x] The same run's caption checks. mc-meet presses Meet's captions toggle once per call, and
  the caption box never shows. Two runs in a row passed every check.
- [ ] AirPods connected and removed mid-call while Meet holds the AirPods mic.
- [ ] A sleep mid-call, through the dev app.
- [ ] 3 real calls, and the CPU check above.
- [ ] A Windows clippy run of the phase 3 code.

## Phase 3b: mic detection

- [ ] `detect` reports which app takes or releases the mic.
- [ ] Teams and Zoom calls reach the call stream.
- [ ] A Meet call ends when its browser releases the mic, not 5 minutes later.
- [ ] Test Anarlog's Windows mic detection (`crates/detect/src/mic/windows.rs`).

Done when:

- [ ] Teams and Zoom each log start and end within 5 s, on the Mac.
- [ ] The same on the PC.

## Phase 3c: Mac video

- [x] `mc-capture video` records one window through ScreenCaptureKit into 2 s HEVC fragments,
  `index.m3u8` and `video.json`. A 60 s run on a fully covered window gave 30 fragments that all
  decode in AVFoundation, at about 1% of one core.
- [x] A `kill -9` of the video helper leaves fragments that play. Repair adds `#EXT-X-ENDLIST`
  and lists the fragments `video.json` had not caught up with.
- [x] 5 s with no callback after a complete frame, while the window is on screen, rebuilds the
  stream, at most 4 times. Checked by injection. A still window, which sends one idle status, and
  a hidden one never rebuild.
- [x] Each part keeps its own size, codec and first frame time, a part shorter than one fragment
  too.
- [ ] Video records only the window id the huddle watcher read. A reopened huddle window reaches
  video as a new hint. Code only; not run on a real huddle.
- [ ] The audio helper's sleep report stops video, and its wake starts video on the audio's wake
  segment. Each video segment records the `audio_segment` it pairs with. Code only.
- [ ] The encoder starts at `call.started`, so its warm-up of about 160 ms drops no frames.
- [ ] A real Slack huddle records its window while covered, with the tray video row.
- [x] Finalize: one MP4 with the video and an AAC mix, track offsets from host time. Run on a
  covered test window with a gap, two segments and a repaired segment; not run on a real huddle.
  The mix is AAC at 48 kbps, since the stems are 16 kHz mono. The file has no faststart. Meet
  placement by browser UTC is code only.
- [x] One video track for parts of different sizes or codecs: a differing part is encoded again in
  the first part's codec and size, letterboxed.
- [ ] Delete fragments after a good finalize.
- [x] 90-day purge of call media: `call.mp4` and the video fragments of calls that ended over 90
  days ago, only after a finalize succeeded, never the stems. Run on synthetic call folders
  only: it kept a young folder, a locked one, one with no `finalize.json`, one with a retry left,
  failed finalizes, fragments with no `video.json`, an open segment and a never-seen folder,
  kept segments with no clock or with a fragment of an unrecorded part, refused links out of the
  calls folder, and finished a purge killed after its record. A first run, a clock a year ahead
  with a relaunch 5 minutes later, and a lost run record with future-dated files deleted
  nothing. After a 3-day jump a call 89 days old stayed and one 91 days old went. A clock set
  back a week aged calls by the wall clock.
- [ ] A Screen Recording grant given mid-call takes effect at the next wake or new huddle window:
  the video process is replaced, so its preflight is not cached. Needs a real revoke and re-grant
  on a Mac; code only.
- [ ] mc-meet tab capture for Meet video, with no Screen Recording grant. One click on the
  mc-meet icon, its context menu entry or its shortcut arms the tab. Built; the fMP4 writer,
  the app's fragment writer and finalize ran on a canvas source only, never on a real call.
- [ ] The tab capture grant lasts while the tab stays on meet.google.com, so later calls in the
  tab start with no click. Untested.
- [x] "Resume recording" restarts Meet tab video with no click. `just meet-e2e` stops and
  resumes through the native messaging protocol, with no popup, and tab capture reopened with no
  app press.
- [ ] Measure `MEET_TAB_CALIBRATION_MS` (60 ms, from the tab capture spike) on the first real
  call.
- [ ] Measure the Meet remote stem trail with Bluetooth output, where the playback latency is
  largest; finalize shifts by the logged output latency plus the calibration.
- [x] The app presses mc-meet's shortcut into the Chrome that hosts it, so Meet video starts with
  no click. The press needs Accessibility, read without a prompt; the click stays the fallback.
  A live `just meet-e2e` run in a fresh test tab with no grant pressed the shortcut while the
  user's own Chrome, a second Chrome process, was the front app; it recorded tab video and kept
  the call recording.
- [ ] The app's shortcut press with a non-US keyboard layout. The app finds the key with
  `UCKeyTranslate` on the current layout; only the US layout has run.
- [ ] Windows: press mc-meet's shortcut with `SendInput`. Chrome starts the host through
  `cmd.exe` there, so the browser process must be found another way first.
- [ ] Window capture fallback for a Meet tab that never got the click.
- [ ] Meet video on Windows. It needs finalize and the purge there first.

Done when:

- [ ] Lips match voices within 100 ms on 3 calls.
- [ ] Revoking Screen Recording shows the warning, and audio still records. Only the debug
  injection ran, never a real revoke: it made the preflight read denied at two segment starts and
  at a stream rebuild inside an open segment, and video reported `permission_denied` each time
  while the audio stems grew at 32 kB/s. The tray row is code only, not seen in a running app.
- [ ] A Meet call records the Meet tab through a tab switch.

## Phase 3d: telemetry

[Design](10-telemetry.md).

### Step 1: core

- [x] `telemetry` crate, portable: `init`, `recorder`, `fields`, `trace`.
- [ ] Builds on Windows. Not run yet.
- [x] JSON lines in the OpenTelemetry log shape: severity, UTC timestamp, body, attributes,
  `trace_id` and `span_id`, a `resource` line at the top of each file.
- [x] Daily files `logs/<service>-<UTC date>.jsonl` for `app`, `native-host`, `mc-capture` and
  `mc`, kept 14 days.
- [x] The file layer at INFO behind a reload filter; `init` returns the handle that sets DEBUG.
- [x] A call's trace id from `call.id`, a pipeline attempt's from `meeting.id` and its input
  revision. Span lines with W3C span and parent ids.
- [x] `mc-capture` opens a `capture.segment` span with the `call.id` from `start`.
- [x] Pipeline step spans carry `meeting.id` and `job.step`.
- [x] Flight recorder: 10 minutes or 8 MB of DEBUG lines from the app's crates. A dump appends to
  its file, up to 32 MB.
- [x] Dumps on a call problem (app and audio helper, through `dump_recorder`), a failed helper, a
  failed pipeline run, a panic and "Save debug report".
- [x] The debug report copies the app's daily files and the new dumps through its redaction.
- [ ] The video helper dumps its recorder on a call problem.
- [ ] A real call problem leaves `recorder-app.jsonl` and `recorder-mc-capture-audio.jsonl` in the
  call's debug folder with the same `trace_id`.

### Step 2: debug mode

- [x] `[debug] enabled_until` in `settings.toml`, tray toggle "Debug mode (24 h)" with a check,
  "Debug until DD Mon HH:MM" in the tooltip while on. The app ends it within 30 s of its end, and
  follows a hand edit within 30 s.
- [x] The app's and the native host's file layers follow it. The audio helper takes `set_debug`
  on stdin after `start` and at each change.
- [x] mc-meet gets `debug_mode` on connect and at each change. Its popup shows the end time.
- [x] A saved report's `call-status.json` has `debug_mode_until`.
- [ ] The video helper follows debug mode.
- [ ] One call in debug mode leaves DEBUG lines in `app`, `native-host` and `mc-capture` files.

### Step 3: metrics and environment

- [x] Gauges every 10 s, or every 1 s in a call, into `debug/<id>/metrics.jsonl`.
- [x] Per-stem audio levels and routes in the heartbeat.
- [x] Caption channel series and acks from mc-meet, per channel and frame kind, with the caption
  channel and press events.
- [ ] The user's speech from mc-meet as `mc.meet.self_speaking`. Meet's speaking indicator has
  nothing stable to read.
- [ ] Host CPU, memory, disk and power. CPU, memory and disk are built; power is not.
- [x] Decision events for source pick and calendar match.
- [x] `env.jsonl` at app start, call start and each change.
- [x] Video frames per second from the window video helper.
- [x] A 50 MB cap per debug folder and a 1 GB cap over all of them.

### Step 4: report and tools

- [x] A report bundle for a call, with the helper's log lines, the job row, the meeting's debug
  folder and the manifests, through the redaction.
- [x] A report bundle for the last 30 minutes from the tray: every service's lines, the
  environment and host metric lines, and an app recorder dump.
- [ ] A report bundle for a job without its call.
- [x] `mc debug timeline <bundle>` in IST, with `--by-minute` and `--service`.
- [x] `mc debug export-otlp <bundle>`.
- [ ] A Collector's `otlpjsonfile` receiver loads an export.

### Step 5: the 0.8.0 caption bug

- [ ] One call in debug mode on the caption bug: missing acks on Meet's channel, captions hidden
  in `control` frames, throttling in hidden-box mode.

### Step 6: detectors

- [ ] One detector trait. The call status rules move under it.
- [ ] `mc debug replay <bundle>`.
- [ ] Each `MC_CAPTURE_INJECT` fault raises its problem and leaves a recorder dump.

Done when:

- [ ] One debug report from a bad call names the failing layer, with no second call.
- [ ] Under 1% CPU and 20 MB of memory per process with debug mode off, on a 30-minute call.

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

Done when:

- [ ] Phase 3 checks pass on the PC.

## Phase 6: extra sources

- [ ] Hinglish calls routed to Soniox.
- [ ] Dropped audio files transcribed.
- [ ] Calendar match from Google Calendar's "secret address in iCal format", with no OAuth sign-in
  and no "Google hasn't verified this app" screen. The URL is a secret, so it goes in
  `settings.toml`. Workspace admins can turn these addresses off.
- [ ] Check Meet caption quality on Indian English and Hinglish.

Done when:

- [ ] A Hinglish call gets a Soniox transcript.
- [ ] An iCal address matches a call with no Google sign-in.

## Phase 7: recording

- [ ] Audio archive and a Drive folder for media. No media in the vault.
- [ ] Recording consent: the tray shows recording state; a per-client "never record" list.

Done when:

- [ ] Video and audio for 3 calls land in the Drive folder, none in the vault.

## Phase 8: hardening

- [ ] Tests, run by the Check workflow.
- [ ] Model download with checksum.
- [ ] Commitment review.
- [ ] Monthly diff of vendored Anarlog crates against upstream.

Done when:

- [ ] Two weeks without a lost meeting.
