# Roadmap

Each phase ends with a check on real calls. Estimates are evenings of about 3 hours.

| # | Phase | Delivers | Done when | Evenings |
|---|---|---|---|---|
| 0 | Skeleton | Workspace with crate stubs and a tray-icon Tauri app, pinned toolchain, rustfmt, clippy workspace lints, cargo-deny, cargo-shear, taplo, typos, biome, rumdl, lefthook hooks, `just check` | `just check` passes on the Mac and the PC; a misformatted file blocks a commit; a GPL crate fails `cargo deny check` | 2 |
| 1 | Tray app and captions pipeline | Tauri tray app with autostart and no Dock icon, caption receiver, Slack huddle captions on the Mac, job store, Google sign-in for work and personal calendars, calendar match, LLM extraction, vault writer, `mc` CLI | 5 real Meet calls and 3 Slack huddles write correct notes with no manual step; the tray lists them; a calendar sign-in still works after 8 days | 6 |
| 1b | mc-meet extension | Our own Meet extension: captions, chat and roster from Meet's data channels with a DOM fallback, native messaging to the app, one event log per call, attendees who joined. Replaces TranscripTonic and the webhook receiver. [Spec](09-meet-extension.md) | 3 Meet calls, one with a second Meet tab, one browser restart and one app quit mid-call, each give one note whose attendees joined the call | 8 |
| 2 | Call detection | One call stream for capture to follow: `call.started`, `call.resumed` and `call.ended`, built from mc-meet's `call_started` and `call_ended` and the huddle watcher | Meet and a Slack huddle each log start within 5 s on the Mac, Meet its end within 5 s and a huddle within 6 s; Meet does the same on the PC | 1 |
| 3 | Mac capture | Tap and mic stems from a supervised `mc-capture` helper, recovery of orphan WAVs at launch | 3 calls, one with AirPods connected mid-call, no gap over 1 s, app and helper under 5% CPU; a `kill -9` of the app or the helper mid-call leaves stems that play | 6 |
| 3b | Mic detection | Calls in native apps such as Teams and Zoom, from the app that holds the mic, on both OSes. Also ends a Meet call at once when the browser crashes | Teams and Zoom each log start and end within 5 s, on the Mac and the PC | 2 |
| 3c | Mac video | A Slack huddle's own window from a supervised `mc-capture video`: ScreenCaptureKit, VideoToolbox HEVC, 2 s HLS fragments, repair at launch. Meet video from mc-meet tab capture. One MP4 per call at finalize | Lips match voices within 100 ms on 3 calls; a `kill -9` of the video helper leaves fragments that play; revoking Screen Recording keeps audio recording | 8.5 |
| 4 | Local STT | Parakeet, VAD, remote diarization, caption-name merge | A 1 h call transcribes in under 10 min on the M2; names match captions on 3 Meet calls | 4 |
| 5 | Windows capture | WASAPI loopback, mic | Phase 3 checks pass on the PC | 4 |
| 6 | Extra sources | Soniox routing, dropped audio files, calendar from a secret iCal address | A Hinglish call gets a Soniox transcript; an iCal address matches a call with no Google sign-in | 3 |
| 7 | Recording | Audio and video archive, Drive folder | Video and audio for 3 calls land in the Drive folder, none in the vault | 2 |
| 8 | Hardening | Tests, model download with checksum, commitment review | Two weeks without a lost meeting | 4 |

Phase 1 took Meet captions from TranscripTonic's webhook. Phase 1b replaced it with mc-meet,
because TranscripTonic split a call into two notes when a second Meet tab opened, and the calendar
invite list stood in for the people who joined.

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
- Meet captions came from TranscripTonic's "advanced" webhook body, saved raw before parsing.
  Phase 1b removed the receiver. The parser stays, so stored payloads still reprocess.
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
- If no account matches or a token is revoked, the note is still written with the title Meet
  showed, and the tray shows "sign in again".

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
  rows with speaker names, participant tiles and the caption language.
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

## Phase 3 detail

- `mc-capture` is a helper binary in `Contents/MacOS/`, shipped as a Tauri sidecar and signed with
  the app's identity. It owns the audio devices and the stem files. The app never links Core Audio
  code, so a crash or hung HAL call in capture never takes the tray down.
- The app runs `mc-capture audio` and talks to it in JSON lines. Each line has a `seq` and a
  `type`, with sorted keys. The `capture-protocol` crate defines every line as a closed enum, and
  each side validates a line once, when it reads it.
- App to helper: `start` with the call id, call folder, segment number, platform and mic moment,
  and `stop`. The mic moment is `call_start` for segment 1 and `mid_call` for every later
  segment: a wake, a rotation, a resume or a helper restart.
- Helper to app: `recording` per stream, `gap`, `heartbeat` every 5 s, `rotated`, `stopped` once
  the segment is closed, `device_changed` when a stream moves to another device mid-segment,
  `mic_unavailable` when the mic is lost and no safe input is left, `permission` with a
  permission's state, `system_sleep` and `system_wake`, and `error` with a closed kind.
- A heartbeat carries each stream's device samples read and 16 kHz samples written.
- Example lines: `{"seq":1,"type":"stop"}` and `{"segment":2,"seq":9,"type":"stopped"}`.
- The helper holds a "prevent idle system sleep" assertion from segment start to segment close.
- The app starts the helper from its own executable's folder, in the app's process group. The
  helper's stderr goes to `mc-capture.log` in the app's log folder.
- A call start or resume takes one segment number for every capture process: one above any
  segment in `audio.json`, `video.json` or a `NNN-` file name.
- If the helper exits, the app restarts it after 1, 2 and then 4 s. A restart takes the next
  segment number only when the failed run left files. A rotation moves the number on too.
- After 3 failures in 60 s the tray shows "Recording failed" and one notification. Restarts then
  wait 30 s, or 5 minutes when a device would not open. The notification comes again only after a
  recording starts or the failures drop below 3 in 60 s.
- The app kills a hung helper. A helper is hung when for 15 s no heartbeat counter grows and
  neither stem file grows.
- To stop, the app sends `stop`, closes stdin and waits 12 s before it kills the helper. The
  helper reads `stop` only after its device opens, which take up to 10 s. If the app exits, the
  helper sees stdin close and closes both WAVs, then exits. Closing stdin stops within 1 s once
  its devices are open.
- The helper log moves to `mc-capture.log.1` at a helper start once it reaches 4 MB.
- One call records at a time. A second open call waits until the first ends.
- Remote audio: a global mono tap (`with_mono_global_tap_excluding_processes` with an empty list)
  in a private aggregate device whose only member is the tap. Adding the output device doubles
  the audio and blocks Zoom and Slack joins. The app does not exclude itself: that broke Zoom on
  macOS 26.
- Mic: `cpal` on the system default input. If the default input is Bluetooth, the built-in mic is
  recorded instead, so the app never forces AirPods into HFP. A virtual input such as
  `ZoomAudioDevice` is skipped the same way.
- With no built-in or wired input, a call start records the Bluetooth or unknown default and
  logs it. A virtual default with no alternative fails the open with a named error. A mid-call
  pick never returns a Bluetooth, virtual or unknown-transport input. An aggregate input counts
  as Bluetooth when one of its active sub-devices is Bluetooth, and as unknown when the HAL
  cannot list them.
- A mid-call segment start with no safe input records the remote stem alone and reports
  `mic_unavailable`. The pick then retries as for a lost mic.
- The helper registers Core Audio listeners for the default input, the default output and the
  device list. A listener only sets a flag; the record loop reads the flags every 50 ms and
  never polls the HAL. A change waits 1 s to settle, or 1.5 s when Bluetooth joined or left
  the route.
- Mic swap: after a settled change the mic rule picks again. A second stream opens beside the
  recording one when the pick names another device, or when the open stream reports its device
  gone.
- The new stream takes over at its first non-zero buffer. After a pick change it also takes
  over after 2 s of zeros, counted from its first buffer. After a lost device it takes over at
  its first buffer.
- A new stream with no buffer within 5 s is dropped, and the swap retries after 15 s. A failed
  open retries after 15 s too. When the old mic was already lost, the dropped stream leaves the
  stem with no device and the helper reports `mic_unavailable`.
- At takeover the old stream stops and its last buffers are written. The stem continues from
  the new stream's first buffer after the old one's end.
- With no safe input left, a changed pick keeps the old stream. A lost mic leaves the mic stem
  with no device: the helper reports `mic_unavailable`, the stem records a gap, and the pick
  runs again every 15 s and at each device change. The tray turns amber with a "without your
  mic" row.
- After 10 s of exact zeros on the built-in mic with the lid closed (`AppleClamshellState`),
  the mic counts as lost, and picks leave it out until the lid opens. The helper carries this
  into the call's next segments.
- Tap rebuild only on evidence: the aggregate device died, the tap's format changed rate, the
  delivered buffers ran 2% off the format's rate for two 2 s windows, the tap delivered nothing
  for 1 s within 10 s of a default output change, or it delivered nothing for 5 s since its
  last buffer or its open. A buffer without a valid host time counts as delivered. Each
  device-list change also reads whether the aggregate is alive. At most 4 rebuilds per episode,
  15 s apart; an episode ends after 60 s without one. A fifth closes the segment with
  `tap_rebuild_limit`, and the app restarts the helper. Silence alone never rebuilds.
- Sleep: the helper registers for IOKit power notifications and holds the sleep until the
  segment is closed, at most 20 s. It reports `stopped` and `system_sleep`, and stays running.
  On full wake it reports `system_wake`, and the app sends `start` with the call's next segment
  number. A dark wake, with no graphics, reports nothing until it becomes a full wake. Host time
  stops during sleep, so the sleep is the span between two segments. A `start` that arrives
  while the Mac sleeps waits for the wake.
- While a failing helper waits 5 minutes for a device that would not open, the app runs
  `mc-capture await-device-change`. A device change ends the wait, and the helper restarts 1.5 s
  later.
- Debug builds of the helper read `MC_CAPTURE_INJECT`, a timetable such as
  `5:mic-lost,12:tap-stall,30:sleep,32:wake`, so the device and sleep paths run without touching
  real devices. Release builds leave the timetable and its test hooks out.
- Every native open runs on its own thread with a 5 s timeout. On timeout the helper exits and the
  app restarts it. Teardown stops the device and waits for running callbacks before it frees
  anything.
- Real-time callbacks only take channel 0 and push it into an SPSC ring of 65,536 samples. Each
  callback also pushes the buffer's host time, sample index and rate into a second ring. The
  helper drains both rings from the first callback and hands them to the stem writer, which
  resamples to 16 kHz with a rubato sinc resampler. The rate comes from each buffer's stamp. A
  new rate rebuilds the resampler.
- Alignment: the writer places each buffer by its own host time. The host-time anchor is never
  reset, not even on a device change.
- Samples the ring dropped become silence of the same length.
- The writer compares each stamp with the one before. A host-time step more than 4 ms beyond
  what the sample count explains marks a stall. Noisy stamps raise the 4 ms limit to 4 times
  their mean step. The writer fills the stall with silence and reports it as a gap.
- The drift is host time minus file position. It goes through a 2 s low-pass into a PI loop. The
  loop stretches or squeezes the audio by at most 1,000 ppm, so a device clock 300 ppm off never
  adds a gap.
- Drift beyond 70 ms with no stall step is filled with silence. This happens on the first buffer
  after a device switch. Cap and OBS smooth within the same 70 ms.
- Audio more than 70 ms ahead of host time is kept, since dropping it would lose speech. The
  manifest lists it under `runs_ahead`. Steering then pulls the file back to host time.
- A gap inside a run ahead also takes up the lead. A rotation carries the lead into the next file.
- A device switch starts the new stream's sample index at 0. The time between the two devices
  becomes a `device_switch` gap.
- Stems are 16 kHz mono 16-bit WAV, 115 MB per hour each. Every 4.5 s the writer fsyncs the
  samples, then rewrites the header and fsyncs again. A power cut then loses at most 5 s.
- A WAV holds at most 37.3 hours, since its sizes are 32-bit. At 24 hours the writer moves both
  stems to the next segment.
- A rotation reads a fresh clock: it closes the old segment and opens the new one. A stopped stem
  stays listed in the closed segment and leaves the new one. Silence past 24 hours opens the next
  file.
- Each call gets a folder in the app data folder, named after the call id. A start, resume,
  helper restart or wake begins a new numbered pair: `001-mic.wav`, `001-remote.wav`, then `002-...`.
- The app picks the segment number. The writer refuses a number at or below any segment in the
  folder, so a finished segment is never opened for writing again.
- Each process keeps its own manifest in the call folder. The audio process writes `audio.json`,
  and the video process writes `video.json`. Finalize reads both.
- `audio.json` has a schema version. Readers accept any version up to their own.
- Each segment entry holds its number, call id, platform, state (`open`, `closed` or
  `recovered`), clock readings at start and close, and close time.
- A recovered segment also holds its `repair`: when, and whether the entry was rebuilt from the
  files alone.
- Each stem entry holds its file, first and last host time, UTC start, `sample_rate_hz`, samples
  in the header, devices, input rates, gaps, runs ahead, ring losses and `stopped_by`.
- A repaired stem also holds its `repair`: `header_samples_before`, `listed_samples_before`,
  `trailing_bytes_cut`, `positions_dropped`, `fixes` and `skipped_reason`.
- The writer replaces `audio.json` atomically at each checkpoint. A failed checkpoint is reported
  and retried at the next one. A failed WAV write stops that stem alone.
- At launch the app repairs every segment left `open` in each call folder. Repair truncates
  each stem to a whole sample. It rewrites the RIFF and data sizes from the file length. It sets
  `samples_written` from the file and marks the segment `recovered`.
- Repair writes only the header and the file length. The next recording takes the next number.
- Each segment start repairs its own folder the same way. A helper restarted after a `kill -9`
  recovers the segment it lost this way.
- A file can end before the positions its entry records, as after a power cut. Repair then drops
  the gaps, runs ahead, devices and rates that start past the new end, and counts them.
- A gap that runs past the new end is shortened to end there. A run ahead that ends past it
  ends there, with its lead read off its slope at that point.
- The first device and the first rate start the file, so they always stay.
- Files with no entry in `audio.json`, or a folder with no `audio.json`, get a rebuilt entry
  marked `recovered`. It has no clock reading, and no call id when no other segment has one.
- Repair checks a stem file fully before its first write. A file it cannot fix, such as one
  without this writer's header, stays as it was.
- The entry of a skipped stem lists `skipped` and the reason. The folder's other stems are still
  repaired.
- Repair writes the header before it cuts a partial sample. A failure between the two leaves a
  file that plays to the same end, and the entry lists the fix already made.
- A segment whose every stem was skipped stays `open`, so the next launch retries it.
- A segment holds an exclusive OS file lock on the folder's `audio.lock` for its whole life.
- Repair takes the same lock before it reads anything. So repair skips a folder whose helper is
  alive.
- The kernel drops the lock when its holder dies, `kill -9` included. So a dead helper never
  leaves a folder locked. A pid file would need a liveness check that a reused pid fools.
- A `kill -9` of the app closes the helper's stdin. The helper then closes its segment and
  exits: 102 ms in the measured run.
- A segment start waits up to 3 s for the lock, as headroom.
- System-audio permission: `TCCAccessPreflight("kTCCServiceAudioCapture")` through dlopen. Mic
  permission comes from `AVCaptureDevice`. The helper checks both at each segment start and
  after a device open fails. It checks the mic again after each run of 10 s of exact zeros, and
  the tap after each run of 60 s of exact zeros during which the mic heard a voice. Nothing
  polls. If
  the private call is missing or answers neither yes nor no, the state is unknown. A denied or
  restricted permission turns the tray amber, adds a tray row that opens its System Settings
  pane, and shows one notification.
- The tray shows the recording dot while stems are open. Each call has an audio row and a "Stop
  recording this call" row.
- "Stop recording this call" closes the segment, and the app logs no caption or chat for that
  call until "Resume recording". The resume starts the next segment. The mc-meet popup has the
  same button.
- The call status board holds each call's stop. The supervisor, the huddle watcher and the tray
  read it there.
- A Meet call's log gets `recording_stopped` and `recording_resumed` lines, so a call reopened
  after a restart stays stopped. A huddle log gets the same lines, and the watcher reads its rows
  while stopped but logs none.
- Both plists get `NSMicrophoneUsageDescription` and `NSAudioCaptureUsageDescription`, and the
  bundle requires macOS 14.4. On 14.2 and 14.3 the tap's permission prompt lands in another
  category.
- Tauri signs with the hardened runtime, which blocks the mic unless the bundle has the
  `com.apple.security.device.audio-input` entitlement. Tauri signs the sidecar with the same
  entitlements.
- macOS files the helper's mic and system-audio grants under the app that launched it, so the
  helper needs no permission of its own.
- Measured on an M2 MacBook Air, macOS 27.0.1: the tap delivers 48 kHz mono 32-bit float and opens in
  under 300 ms. The helper uses 0.4% of a core with the tap open, 0.6% with the mic too, before
  resampling.
- The tap keeps running at 48 kHz across a switch to AirPods Pro, with no dropped samples. The
  switch also moves the default input to the AirPods; an open built-in mic stream stays on the
  built-in mic. The AirPods mic, once open, records at 24 kHz mono and the tap stays at 48 kHz.
- No echo cancellation: it was 63 to 76% of Muesli's CPU. Phase 4 drops mic segments whose text
  matches an overlapping remote segment.

### Build order

1. Spike: plist strings, minimum version, a bare `mc-capture` that records 60 s. Check that macOS
   attributes its prompts to Meeting Capture. Measure the tap's format, whether it survives a
   switch to AirPods, the AirPods HFP rate, and CPU with only the tap open.
2. `capture` crate: `CallAudioSource`, tap and mic sources, ring with host-time stamps, timed opens,
   teardown.
3. `stem-writer`: resampling, gap fill, clock steering, WAV fsynced every 4.5 s, `audio.json`.
4. Helper protocol, app supervision, tray rows. The app subscribes to the call stream early in
   `setup()`, so it sees the calls ended at launch.
5. Device listeners, the Bluetooth rule, mic swap, tap rebuild on stall, permission checks.
6. Orphan repair at launch. Then 3 real calls, AirPods mid-call, `kill -9` of each process.

## Phase 4 detail

### Desert Ant models on the Mac

[Desert Ant Labs](https://desertant.com/models/) ships on-device speech models that run on the
Neural Engine through Core ML. Three of them cover phase 4 and phase 6 work. Each is free up to
100k monthly active devices.

| Model | Job | Figures from the vendor | Replaces |
|---|---|---|---|
| Voz | Parakeet TDT 0.6B v3 compiled for the Neural Engine, 467 MB | 10 min of audio in 2.4 s on an M1 Mac mini; 7.40% average WER, 11.84% on AMI | sherpa-onnx Parakeet int8 on the CPU |
| Who | Diarization: speaker numbers with start and end times, no names | AMI DER 12.6% against 13.1% for pyannote community-1; 950x real time on an M5 Pro | Remote-stem diarization |
| Ear | Spoken language ID over 99 languages, from audio | 250 ms per check | whatlang on transcript text, for Soniox routing |

Our own Rust crate, `desert-ant`, runs the published weights on both OSes. The official SDKs are
Swift, Kotlin and JavaScript only, and Voz has no native Windows build.

- Mac: the `.mlmodelc` builds through Core ML (`objc2-core-ml`), on the Neural Engine.
- Windows: the `.tflite` builds that Voz, Who and Ear already publish, through LiteRT on the CPU.
  The vendor measures this at 3 to 7 times slower than the Neural Engine.
- The crate ports the SDK's mel front end, window cutting and decoding from `desert-ant-core`.
- Weights download from Hugging Face on first use, with a checksum, like the phase 8 model download.
  The repo never holds them.

Licence: the models and SDK are under the Desert Ant Labs Source-Available License 1.0.

- It allows use, change and shipping inside an app. Use is free under 100,000 monthly active
  devices per platform.
- Changes and derivatives stay under that licence. `desert-ant` is a derivative, so it carries
  that licence, not MIT, and its README says so, like the vendored Anarlog crates.
- It forbids distributing the models or the SDK on their own.
- It asks for credit where users can find it: the README and the settings window.

Limits:

- Who weakens past 4 speakers per 5 minutes: VoxConverse DER is 24.7% with 9 or more speakers.
- Voz does not detect language and gives confident wrong text on unsupported speech. Its 25
  languages exclude Hindi, so Hinglish still goes to Soniox.

Plan: start phase 4 with Who for diarization and Voz for STT, through `desert-ant`. First check
Voz's speed and WER against sherpa-onnx Parakeet on 3 recorded calls, on the Mac and the PC.
Keep sherpa-onnx as the Windows path if Voz on LiteRT is slower there.

Schemer (typed JSON extraction) does not fit: it copies spans and does not summarise, and it is in
closed beta.

## Open questions

- Whether the akaiketech.com Workspace admin allows unverified third-party apps to read calendars.
  If not, add a second OAuth client with an "Internal" consent screen in the akaiketech.com org.
- Anarlog's docs say mic detection is macOS and Linux only, but `crates/detect/src/mic/windows.rs`
  exists. Test it in phase 3b.
- Parakeet int8 speed on the M2 CPU. The only CPU figure is 13.7x real time on a Ryzen 4750U.
- Meet caption quality on Indian English and Hinglish.
- Whether Ear tells Hinglish from English reliably. Its page names no Indian language and says
  nothing about mixed-language speech.
- Whether a Rust crate for LiteRT on Windows is maintained, or `desert-ant` needs its own bindings
  to the LiteRT C API.
- Haiku 5.5, Sonnet through Claude Code and GLM-5.3-Flash on five real transcripts.
- Codex as a fourth provider, through `codex exec`.
- Recording consent: a per-client "never record" list.

## Risks

| Risk | Mitigation |
|---|---|
| The global tap records notification sounds and music | VAD drops most of it; move to a per-process tap later |
| On speakers, remote voices also land in the mic stem | Drop mic segments that match an overlapping remote segment; Anarlog's `aec` crate later |
| A macOS update changes tap permissions | Check the tap at launch and warn in the tray |
| Parakeet on the Windows CPU is slow | Run STT when idle, or send Windows stems to Soniox |
| Vendored Anarlog code drifts from upstream | Record the upstream commit per vendored crate; diff monthly |
