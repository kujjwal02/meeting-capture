# mc-meet extension

mc-meet is our Chrome extension for Google Meet. It reads captions, chat and the participant
list in each call you join, and streams them to the tray app as they happen. On macOS it also
records the Meet tab's video for the app. The app writes one note per call.

## Parts

```mermaid
flowchart LR
    PAGE["Meet tab<br/>page hook + content script"]
    SW["mc-meet<br/>service worker"]
    HOST["meeting-capture<br/>in host mode"]
    APP["meeting-capture<br/>tray app"]
    LOG[("browser-calls/<br/>session JSONL")]

    PAGE -->|chrome.runtime port| SW
    SW <-->|native messaging, stdio| HOST
    HOST <-->|Unix socket or named pipe| APP
    APP --> LOG
    LOG -->|call ends| PIPE["pipeline"]
```

| Part | Where | Job |
| --- | --- | --- |
| Call events | `extension/call-events.js`, isolated world and service worker | Builds every event the extension sends, one function per `kind`, in the shapes below. |
| Page hook | `extension/page-hook.js` and `extension/page/wire.js`, MAIN world, `document_start` | Wraps `RTCPeerConnection` and decodes Meet's `collections`, `captions`, `captions_v2` and `meet_messages` data channels. Opens the two caption channels itself. |
| Content script | `extension/content/`, isolated world | Finds call start and end, reads the DOM when the channels give nothing, and owns the call session for its tab. |
| Session rules | `extension/session-rules.js`, service worker | Decides what each tab message, tab change and alarm does to the worker's sessions. Numbers events. Makes no `chrome.*` calls and reads no clock, so it also runs in Node. |
| Service worker | `extension/background.js` | Stores the session state and queued events in one write, holds the native port, resends after a reconnect. Shows each Meet tab's call status on the toolbar icon. |
| Tab video | `extension/tab-video.js`, service worker | Picks the Meet tab whose call wants video, asks for its stream id, tells the app when Chrome refuses so the app can press the shortcut, asks for the click as the fallback, forwards fragments to the app. |
| Offscreen capture | `extension/offscreen/`, offscreen document | Opens the tab stream, plays its audio back, encodes with WebCodecs and cuts fMP4 fragments (`fmp4.js`, which also runs in Node). |
| Popup | `extension/popup/` | The active tab's call status and video state, a "Stop recording this call" or "Resume recording" button, and a "Save debug report" button. |
| Host mode | `crates/native-host`, run by the app binary | Chrome starts `meeting-capture chrome-extension://<id>/`. `main` sees the argument and relays frames between stdio and the app socket before Tauri starts. |
| Call log | `crates/app/src/browser_calls.rs` | Listens on the socket, appends each event to the session log, acks it, ends calls, hands logs to intake. |
| Parser | `crates/sources/src/browser_call.rs` | Turns a session log into a `Transcript`. |

## Identity

- Extension ID: `pbpjeihgfeefjehjnjknnefomhkblcfk`. The `key` in `extension/manifest.json` pins it.
  The private key is not needed to load unpacked.
- Native host name: `com.meetingcapture.app`. The manifest's `allowed_origins` lists only
  `chrome-extension://pbpjeihgfeefjehjnjknnefomhkblcfk/`.
- The app writes the host manifest on every start, with `path` set to its current executable.
  - macOS: `<browser config>/NativeMessagingHosts/com.meetingcapture.app.json` for each of
    Google Chrome, Chrome Beta, Chrome Canary, Chromium, Brave and Microsoft Edge whose config
    folder exists.
  - Windows: one manifest file in the app's data folder, and a
    `HKCU\Software\<browser>\NativeMessagingHosts\com.meetingcapture.app` key per browser pointing
    at it.

## Socket

- macOS: Unix socket `browser-calls.sock` in the app's data folder, mode 0600. The app removes a
  stale socket file at start.
- Windows: named pipe `\\.\pipe\meeting-capture-<user name>`, remote clients rejected.
- `MEETING_CAPTURE_HOME` moves the data folder for both the app and host mode, as it does for `mc`.
  On Windows it adds `-<8 hex digits>` from the data folder's SHA-256 to the pipe name.
- The app drops a connection whose first frame is not `host_hello` with the extension's origin.
- Host mode logs to `native-host-<UTC date>.jsonl` in the app's logs folder. Stdout carries only
  frames.
- One socket connection per extension port. The app serves several at once: one per Chrome
  profile.

## Frames

- Chrome to host and host to Chrome: Chrome's native messaging framing, a 32-bit length in native
  byte order, then UTF-8 JSON. A message to Chrome is at most 1 MB.
- Host to app and app to host: the same frames with a little-endian length. The host passes every
  frame through unchanged, except the first one it sends the app:
  `{"type":"host_hello","origin":"chrome-extension://.../","host_version":"0.2.0","browser_pid":123}`.
  `browser_pid` is the host's parent process, which on macOS is the Chrome that started it. It is
  null on Windows, where Chrome starts the host through `cmd.exe`.
- A frame closes the connection when it is over 8 MiB from the extension or over 1 MiB towards
  it. Chrome sends a host up to 64 MiB but takes at most 1 MB from one.
- When the socket is absent, host mode starts the app and retries the connection every 250 ms for
  10 s. Then it sends `{"type":"app_unavailable"}` to Chrome and exits.

## Messages

Each message is a JSON object with a `type`. Times are RFC 3339 UTC strings. Unknown `type`s and
unknown fields are ignored on both sides, so either side can be newer.

### Extension to app

| `type` | Fields | When |
| --- | --- | --- |
| `hello` | `protocol` (integer, now 1), `extension_version`, `sessions` (the session IDs the worker has open) | First message on each port. The app counts the port as the holder of those calls. |
| `event` | `seq`, `session_id`, `event` | Each call event. `seq` grows by one per event for the whole install and survives a worker restart. A new install starts it at the install time in microseconds, so a reinstall mid-call never repeats a `seq` in the session it resumes. |
| `frame_failed` | `session_id`, `channel` (`captions` or `captions_v2`), `at`, `reason`, `skeleton`, `byte_length`, `bytes_base64` (the frame as received, or null above 256 KB) | A caption channel frame that did not decode. At most 20 per channel per call from the page, and 20 per session from the content script. Failures on `collections` and `meet_messages` stay in the page: a chat or roster frame with nothing to read is common. Not queued: sent only while the app is connected. |
| `telemetry` | `schema` (now 2), `session_id`, `at`, `channels` (each `label`, `id`, `serial`, `is_own`, `started_at`, `raw`, `parsed`, `control`, `rejected`, `acks`), `events` (each `at`, `name`, `attributes`) | Once a second while the tab is in a call. See "Telemetry". Counts are not queued; events wait in the worker until the app is welcomed. |
| `telemetry_frames` | `session_id`, `frames` (each `channel`, `channel_id`, `is_own`, `at`, `byte_length`, `bytes_base64`) | Once a second in debug mode, with every caption channel frame of that second. None while the call is stopped. Not queued. |
| `pages_away` | `session_ids`: every session whose tab closed, reloaded or crashed and may still come back | After `welcome`, and whenever the list changes. Not queued. A call all of whose sessions are away shows "the Meet tab closed or reloaded", with no notification. |
| `page_snapshot` | `session_id`, `snapshot` | Reply to `snapshot_request`. See "Debug material". |
| `stop_recording` | `session_id` | The popup's "Stop recording this call" button. The app handles it as the tray row of the same name. |
| `resume_recording` | `session_id` | The popup's "Resume recording" button. |
| `save_debug_report` | `session_id` (the popup's tab session, or null), `with_text` | The popup's "Save debug report" button. The app answers `debug_report_failed` for a `session_id` that is not 1 to 64 letters, digits or hyphens. |
| `video_state` | `session_id`, `state` (`needs_click`, `capturing`, `stopped` or `failed`), `detail` (null, or for `stopped` one of `not_wanted`, `track_ended`, `tab_gone`, `app_silent`; for `failed` the error text), on `stopped` `capture_id` and `last_index` (the last fragment sent, 0 for none), and on `needs_click` `meet_tab` (`is_tab_active`, `is_window_last_focused`) and `shortcut` (the bound key from `chrome.commands.getAll()`, such as `⌘⇧U`, or empty) | Each change of the tab capture for a call that wants video. A `needs_click` goes again when the tab is activated or a Chrome window takes focus. Held with the fragments while the port is down. |
| `shortcut_press_ready` | `session_id` | Reply to `shortcut_press_expected`, sent only by the worker that holds the session, after it read the tab's facts again and they hold. The app presses into the `browser_pid` of the port that sent it. |
| `shortcut_press_refused` | `session_id` | Reply to `shortcut_press_expected` when the tab no longer waits for the invocation or no longer sits where the press lands. A `needs_click` with the new facts goes first. |
| `shortcut_press_unseen` | `session_id`, `reason` (`no_command` or `other_tab`) | No command reached the worker within 2 s of its `shortcut_press_ready` (`no_command`), or it reached another tab (`other_tab`). The key went to a window the worker cannot see. |
| `shortcut_press_noted` | `session_id` (the target's, held by another worker) | Reply to `shortcut_press_expected` from every worker that does not hold the session. It ignores a command for the next 2 s. |
| `shortcut_press_landed` | `session_id` (the target's, held by another worker) | A command came within 2 s of a `shortcut_press_expected` for a session this worker does not hold. The worker ignores the command, and the app counts the press as missed. |
| `video_fragment` | `session_id`, `capture_id` (16 hex digits, new per stream), `part`, `codec` (`hevc` or `h264`), `width_px`, `height_px`, `init_base64`, `index`, `first_frame_utc_ms`, `duration_ms`, `frame_count`, `part_clock_offset_ms` (UTC minus `performance.now()` at the part's first frame) and `clock_offset_ms` (the same at this fragment), `playback_latency_ms` and `playback_base_latency_ms` (or null), `bytes_base64` | One fMP4 fragment, about every 2 s while capturing. Not queued or acked: the worker holds the last 30 while the port is down. The app drops a repeat of a `capture_id` and `index` it wrote, a late fragment of a capture a newer one replaced, a fragment older than the call's tab video task, and one whose part predates a sleep the audio reported. |

`event.kind` is one of the kinds below. `extension/call-events.js` builds each of them.

| `kind` | Fields |
| --- | --- |
| `call_started` | `at`, `platform` (`meet`), `meeting_code`, `title`, `tab_id` |
| `caption` | `at`, `device_id`, `utterance_id`, `version`, `speaker`, `text`, `source` (`channel` or `dom`) |
| `chat` | `at`, `message_id`, `device_id` (channel only), `sender`, `text`, `source` |
| `roster` | `at`, `participants`: list of `{device_id, name, is_self}`, the whole list each time it changes |
| `health` | `at`, `source` (`channel`, `dom` or `none`), `captions_on` (Meet's captions button is on, and Meet does not refuse captions), `caption_box_shown` (Meet's caption box is on screen), `captions_unavailable` (Meet refuses captions), `caption_frames_failing` (caption frames fail to decode, with no caption decoded since), `parse_failures`, `detail` |
| `recording_stopped` | `at`; the app writes it, with no `seq` |
| `recording_resumed` | `at`; the app writes it, with no `seq` |
| `call_ended` | `at`, `reason` (`left`, `tab_closed`, `kicked`, `page_unload`; the app writes `idle` and `app_restarted`) |

- `caption`: `utterance_id` is a string and `version` an integer. `at` is when the utterance first
  appeared, so every version of one utterance has the same `at`. A `caption` with a higher
  `version` for the same `device_id` and `utterance_id` replaces the earlier text. The extension
  sends a new version only when the version grows or the text changes.
- `speaker` is empty when the device had no name yet. The parser then takes the name for
  `device_id` from the attendee list, which holds the first name any `roster` in the log gave
  that device. A roster that arrives late still names the earlier lines. `sender` on `chat`
  follows the same rule. A `chat` with no `device_id` keeps an empty `sender`.
- The DOM source has no device or utterance IDs. It uses the speaker name as `device_id`, and
  `dom-<page token>-<turn number>` as `utterance_id`. The page token keeps IDs unique after a
  reload.
- Channel `device_id`s look like `spaces/<id>/devices/<id>`. Legacy `captions` frames get a `v1-`
  prefix on `utterance_id`. They are dropped once a `captions_v2` frame has arrived.
- `roster` lists the people in the call now: devices with status left (6), removed (7) or refused
  (4) are dropped, and so are screen-share devices. The app unions every roster of the session.
- `health` is sent at call start and whenever one of its fields changes. `detail` is null, or the
  reason for the DOM fallback, or a parse-gap report: after 10 failed frames in a row on one
  channel, it names the failure reasons and a skeleton of the last frame (tag and wire type tree
  with lengths, no text). `parse_failures` counts failed frames on all channels.
- `captions_unavailable` is true when Meet's captions button is disabled, or its own label says
  "Live captions are temporarily unavailable". A hover tooltip with that text does not count: it
  shows only under the pointer. The text match is English only; other UI languages rely on the
  disabled button.
- `source` is `channel` only after a caption arrived on the caption channels the page has open.
  When Meet opens a new meeting connection, the hook opens new caption channels and `source`
  goes back to `none` until one of them carries a caption.
- After each `welcome` the worker tells every Meet tab, and each tab sends `health` again: a
  restarted app has none for the call.
- At each `call_status` the worker sends each Meet tab with a session a `recording_state` with
  `is_stopped`. A tab whose call is `stopped` sends no caption or chat.
- The app writes `recording_stopped` and `recording_resumed` in the call log at each stop and
  resume. It refuses both kinds from the extension. While a call is stopped, the app acks and
  drops its captions and chat.
- A call reopened from its log, after a restart or a rejoin, is stopped when the log's last stop
  or resume is a stop.
- The popup port is accepted only from the extension's own popup page.

### App to extension

| `type` | Fields | When |
| --- | --- | --- |
| `welcome` | `protocol`, `app_version`, `folder_version` (the version in the folder the app installs mc-meet into, or null), `open_sessions` (session IDs the app still has open), `settings` | Reply to `hello`. |
| `ack` | `seq` | After the event is appended to its log. |
| `settings` | `settings` | After a save in the settings window, to every welcomed port. |
| `app_unavailable` | none | From host mode when the app did not start in 10 s. |
| `call_status` | `calls`: one entry per open Meet call, below | After `welcome`, and whenever a call's status changes. |
| `snapshot_request` | `session_ids` | When a call gets a problem, at most 5 times per call. |
| `shortcut_press_expected` | `session_id` | Before the app presses mc-meet's shortcut for that session's tab. To every welcomed port: the one that holds the session answers ready or refused, and every other one answers noted. The app presses only after every port welcomed at the notice answered, within 1 s. |
| `debug_report_saved` | `path` | Reply to `save_debug_report`. |
| `debug_report_failed` | `error` | Reply to `save_debug_report`. |
| `debug_mode` | `until`: the end of debug mode in RFC 3339 UTC, or null when it is off | After `welcome`, and at each change. See "Telemetry". |

Each `call_status` entry has `session_ids`, `title`, `state` (`waiting`, `recording`, `problem`
or `stopped`), `reason` and `reason_text` (null unless `problem`), `started_at`, `started_at_label`,
`recording_since_label`, `caption_count`, `last_caption_at`, `caption_source` (`channel`, `dom`,
`none` or null), `problems` (the last 3, oldest first, each `{reason, reason_text, at,
at_label}`), and `is_video_wanted` (the app records this call's Meet tab video now). Labels are
clock times such as `21:02 IST` in the app's settings time zone.
`reason` is one of `captions_off`, `meet_captions_unavailable`, `caption_parse_failures`,
`captions_silent`, `no_captions`, `extension_disconnected`, `meet_page_away` and
`huddle_captions_unreadable`. mc-meet shows `reason_text` and needs no other.

The extension keeps each event in `chrome.storage.local` until its `ack`, and resends unacked
events in `seq` order after `welcome`. The app ignores an event whose `seq` its log already has,
and acks it again. An event with an invalid `session_id` (1 to 64 letters, digits or hyphens) or
invalid fields for its `kind` is acked and dropped, since a resend cannot fix it. An event the app
could not write gets no `ack`.

`settings` holds `hide_captions`, the app's `[meet] hide_captions`. The worker stores it in
`chrome.storage.local`, and the content script reads it at load and on each change. Without a
stored value the content script hides the caption region.

## Sessions

- A session is one call in one tab. The content script creates `session_id` (a UUID) when the
  call starts, and keeps it in `sessionStorage` with the meeting code. A reload of the same
  meeting in the same tab within 5 minutes reuses the session. A second Meet tab never touches
  the first tab's session.
- The page is in a call when the Leave call button exists. The content script ends the call when
  that button is gone for 3 s: `kicked` if the roster showed the local device as removed, else
  `left`.
- The worker ends the call `tab_closed` when the tab closes.
- When the page unloads (reload, navigation, crash), the worker waits 5 minutes. If the same tab
  resumes the session, the call goes on. Otherwise the worker ends it `page_unload`, with `at` set
  to the unload time. A new session in that tab ends the old one at once.
- When the worker starts and a session's tab no longer exists, the session gets the same 5-minute
  wait. This covers a browser restart or crash, and a session left open while the extension was
  off.
- The worker sets the tab to `autoDiscardable: false` while the call runs.

## Extension internals

- Caption source. The hook opens `captions` and `captions_v2` on the peer connection that carries
  `collections`. Meet streams nothing to them until something asks for captions, so the content
  script presses Meet's captions button once, or Meet's C shortcut where the account has the
  "Captions and translation" button. The stream then goes to the hook's channels, and Meet's
  caption box never shows. About 30 s later Meet's captions button may read on, still with no
  box. Meet sometimes ignores the press: the button never reads on and no stream comes. A second
  press after one Meet took would stop the stream. So mc-meet presses again only while the
  button has not read on 45 s after the last press, at most 3 times per call. Each press, with
  its reason, the control state before it, `document.hasFocus()` and the page visibility, goes
  into the page snapshot of a debug folder as `captions_presses`. Some Meet builds also open
  their own negotiated `captions_v2` channel at call start. The hook reads it too. When no
  caption arrives within 20 s of opening the channels, or 10 caption frames in a row fail to
  decode with no good caption in the last 20 s, the content
  script reads the caption region in the DOM. That region exists only while the user has Meet's
  captions on. The first channel caption after that switches back to the channel and stops the
  DOM reader. Channel captions count while recording is stopped, so a stop does not start the
  DOM reader. `extension/content/caption-source.js` holds these rules. It reads no DOM and no
  clock, so it also runs in Node. `content/main.js` feeds it timestamped facts and runs the
  reader it picks.
- Hidden captions. Meet's caption box covers a shared screen. With `hide_captions` on, the
  caption region gets `display: none` while the DOM reader runs. Meet keeps writing captions
  into the hidden region. Turning captions off makes Meet close its own caption channel, and the
  page hook blocks `close()` on caption channels it did not open.
- The DOM reader makes one utterance of each speaker turn. Meet did not trim a 1200-character
  monologue in the caption region, so one speaker talking alone gives one caption line, and a call
  with fewer than 3 lines writes no note.
- Page hook to content script. The hook sends spec events (`caption`, `chat`, `roster`) as
  `event` messages. It sends its own notices (`parse_gap`, `self_removed`, `frame_failed`) as
  `notice` messages. Only `frame_failed` of a caption channel reaches the app, as `frame_failed`.
- Roster. From `collections` and the `SyncMeetingSpaceCollections` response. When neither gives a
  roster 30 s into the call, the content script reads the People panel if it is open, else the
  video tiles, every 10 s.
- Chat. From `collections`, `meet_messages` and the `CreateMeetingMessage` response, deduplicated
  by message name. There is no DOM chat reader.
- Content script to worker. One port per tab. Each event carries a number `n` per session, kept in
  `sessionStorage`. The worker stores the event, replies `stored`, and drops any `n` it has stored
  before. The content script resends unstored events after a reconnect. It pings every 20 s. If
  the worker already ended the session (the page came back after 5 minutes), it replies
  `session_ended`, and the content script starts a new session.
- Worker to app. The worker opens the native port when it has a call or queued events, and closes
  it after the ack of the last `call_ended`. On a disconnect or `app_unavailable` it reconnects
  after 1 s, doubling to 60 s, and an alarm retries every 30 s.
- Extension update. The worker injects fresh content scripts into open Meet tabs. They resume the
  tab's session. The old page hook keeps running until the tab reloads.
- Tab video. Chrome lets `chrome.tabCapture.getMediaStreamId` read a tab only after the user
  invokes mc-meet on it: the toolbar icon (opening the popup counts), the "Record this tab's
  video" context menu entry, or the `toggle-recording` command, Command+Shift+U on the Mac and
  Alt+Shift+V elsewhere unless the user rebinds it. The grant should last while the tab stays on
  meet.google.com, so one invocation would cover later calls in that tab; this is untested.
  When the app marks a call
  `is_video_wanted`, the worker asks for the stream id. A refusal sends `needs_click` and shows
  `REC?`.
- Automatic start. On the Mac the app answers `needs_click` by pressing mc-meet's shortcut into
  the Chrome that started the host, as a real key event posted to that Chrome's pid
  (`browser_pid`). Chrome counts it as a `commands` invocation, so capture starts with no
  click. A key posted to the pid reaches Chrome while another app is in front, and Chrome runs
  the command on the active tab of its last focused window. Measured: with the user's own
  Chrome in front, the press armed the test Chrome's Meet tab and capture opened 98 ms later.
  So the app presses only when the Meet tab is the active tab of Chrome's last focused window,
  and never moves focus. It also needs the Accessibility grant, read with `AXIsProcessTrusted`
  only; the app never raises the prompt. It maps the bound key from `shortcut` and finds it
  on the current keyboard layout with `UCKeyTranslate`; an empty or unknown key, or a letter
  no key types, leaves the click. Before the press the app sends `shortcut_press_expected`.
  The worker reads the tab's facts again and answers `shortcut_press_ready`, or
  `shortcut_press_refused` after a `needs_click` with the new facts.
- Where the press lands. Every Chrome profile runs in one process, and the worker cannot see
  another profile's windows, an installed app's window or incognito. A press can land in one
  of them, where no command is bound and the key reaches the page. So the app presses only a
  shortcut that holds Command or Control, which types no text there; Option+Shift+V would
  type "◊". The app presses within 1 s of its notice or not at all, so each worker's press
  window opens at the notice and lasts 2 s, which leaves the key 1 s to arrive. The worker
  reports `shortcut_press_unseen` when no command arrives within 2 s of its ready reply, or
  when the command reaches another tab, which it leaves alone. A worker in another profile
  that also runs mc-meet hears the same `shortcut_press_expected`, since the notice goes to
  every welcomed port. It answers `shortcut_press_noted`, ignores a command for the next 2 s,
  and reports `shortcut_press_landed`, so the press cannot stop a call in that profile. The
  app presses only after every port welcomed at the notice answered, so no worker can get the
  key before its window opens; a port that does not answer within 1 s, such as an older
  mc-meet, costs that press. After one missed press the app stops pressing for the call. A
  worker with no welcomed port does not hear the notice. Its stored `call_status` names the
  calls of every profile, so the worker drops it when it closes an idle port, and its command
  stops nothing until a `call_status` comes on a welcomed port. Without that, the app's press
  for profile A, landing in profile B, would reconnect B and stop A's call. The target then
  reports the press unseen, and the app does not press again for that call. The app presses
  only into a pid whose bundle id is Chrome, Chrome Beta, Dev or Canary, or Chromium: the host
  reports the pid, and any process of the user can reach the app's socket.
- Retries. When a condition fails, the app tries again on the next `needs_click`, which the
  worker sends on each tab activation and each Chrome window focus change, and 3 s after a
  press that opened no stream or a notice that a port did not answer in time. It presses at
  most once per 3 s, and stops after 3 presses with no stream, after an unseen press, after the
  stream opens, or when the call ends. Each press and each skip is logged as
  `video.shortcut_sent` or `video.shortcut_skipped` with `video.press_skip_reason`; the 3-press
  limit and the unseen press are logged once.
- The click stays the fallback. One notification per tab asks for a click on the icon or the
  context menu entry. It
  waits until the call has a `caption_source` of `channel` or `dom`, or a problem reason: a
  notification can take the page's focus, and Meet may then ignore mc-meet's captions press.
  The page snapshot records `document.hasFocus()` at that press. The invocation starts capture at
  once. The icon and the context menu entry also retry video that failed. On a tab that waits
  for the invocation, the command only arms it, whoever pressed it. On any other tab it stops
  a call that records, waits for captions or has a problem, whatever its video state.
  Pressed on a tab outside Meet, it stops every such call. It resumes a stopped call, and with
  no call it arms the tab as the icon does. A stop or resume from the command is held while
  the native port is down.
- App silence. The app repeats `call_status` at least every 5 s while a call wants video. Each
  fragment checks the time since the last one on the monotonic clock; after 10 s the worker
  stops the capture with `app_silent`.
- Offscreen capture. One offscreen document (reasons `USER_MEDIA` and `AUDIO_PLAYBACK`) takes the
  stream id at a fixed 1280x720, so Chrome letterboxes the tab. A VideoEncoder in realtime mode
  encodes 15 fps as HEVC, or H.264 where HEVC has no encoder, with a keyframe every 2 s. Each
  fragment runs from one keyframe to the next. A part takes `Date.now() - performance.now()` at
  its first frame and places its frames by `performance.now()` and frame timestamps. When that
  offset moves by over 1 s (a sleep stops `performance.now()`), a new part starts with fresh
  decode times. Each fragment sends its part's offset and the current one. The app keeps the
  first part's offset for later parts and logs the step, unless the audio reported a sleep or
  started a new segment since; then the new part's offset holds. A fragment that would take a
  new offset is dropped when its own offset differs from its part's by over 1 s: it is the
  pre-sleep part's open fragment, sent after the wake. Such a fragment is also dropped when it
  ends before the audio's last start, which catches one the reconnect buffer held across the
  sleep. An AudioContext plays the tab
  audio, since capture mutes the tab, and each fragment carries its `outputLatency` and
  `baseLatency`. A stop flushes the last fragment and replies with its index. The app drops
  the whole fragment that holds the user's stop, so no video after the stop is kept; up to 2 s
  before the stop are lost with it. A stop because the tab left Meet drops the open fragment
  and the encoder's queue instead, since those frames may show another page.
- Permissions: `nativeMessaging`, `storage`, `unlimitedStorage` (a long call queued while the app
  is down), `alarms`, `scripting` (the update injection), `notifications` (the app is not
  running, or the tab needs its click), `tabCapture` and `offscreen` (tab video),
  `contextMenus` (the record entry), host `https://meet.google.com/*`.

## Call status

The app decides each call's status. mc-meet only shows it, and decides one thing itself: that the
app is not reachable.

| State | Icon | Badge | Title |
| --- | --- | --- | --- |
| No call in the tab | grey ring | none | `mc-meet: connected` |
| Waiting | grey ring | `…` | `Waiting for captions` |
| Recording | red dot | `REC` | `Recording since 21:02 IST` |
| Problem | amber | `!` | `Not recording: <reason>` |
| Problem while tab video records | amber | `REC!` | `Recording video; not recording captions: <reason>` |
| Video needs the click | amber | `REC?` | `Click to record this tab's video` |
| Stopped by the user | grey ring | none | `Recording stopped` |
| App not reachable | grey ring, crossed out | `OFF` | `Meeting Capture is not running` |

- The worker sets the icon, badge and title per Meet tab with `chrome.action`. It keeps the last
  `call_status` and the app's reachability in `chrome.storage.session`, which the popup reads.
- The app is not reachable after `app_unavailable`, a failed `connectNative`, or a native port
  that closed before `welcome`. With a call open, the worker then shows one Chrome notification
  per outage.
- The popup shows the state and reason, the title, start time, caption count, the time since the
  last caption, the caption source (`call` for the channels, `page` for the DOM), the app and
  mc-meet versions, and the last 3 problems. An open popup keeps the native port open. Opening
  it with no call starts the app if it is not running, since host mode starts the app.
- "Stop recording this call" in the popup sends `stop_recording` with the tab's session. Once
  the call is `stopped`, the button reads "Resume recording" and sends `resume_recording`.
- "Save debug report" in the popup sends `save_debug_report` with the tab's session. The app
  writes the report and reveals it in Finder or Explorer. With no answer in 30 s, the popup says
  the report was not saved.

## Telemetry

mc-meet sends caption channel telemetry to the app once a second while its tab is in a call. It
holds no caption text, names, titles or meeting codes. Every string in it is a code from a fixed
list. The app keeps only the attributes it lists for each event name, each with a value of the
right kind: a boolean, a whole number, or one of the event's codes. It logs how many it dropped
in `mc.meet.dropped_attribute_count`.

- The page hook counts each caption channel apart: Meet's own, such as its negotiated
  `captions_v2` with id 125, and the two the hook opens. It counts `raw` frames on arrival, then
  each frame's outcome: `parsed`, `control` or `rejected`. A text frame and a frame the reader
  throws on count as `rejected`. It also counts the acks it sends. The frame path does integer
  increments only. The hook posts once a second.
- Each tracked channel gets a `serial`, counted from 1 in the page. A rejoin in the same tab can
  bring a channel with the label and id of an old one; the serial keeps their counters apart.
- `RTCPeerConnection.close()` closes its channels and fires no `close` on them. The hook checks
  each channel's `readyState` at each post. A closed channel goes into one last post with its
  final counts and a `closed` event, then the hook drops it.
- The worker sends the counts as `channels`. The app builds the metric row from these typed
  fields: `mc.captions.frames` per channel and frame kind, and `mc.captions.acks` per channel,
  with the attributes `mc.caption.channel.label`, `mc.caption.channel.id`,
  `mc.caption.channel.serial` and `mc.caption.channel.is_own`. The counts are cumulative from
  the channel's start. `extension/telemetry-rows.js` builds the messages.
- The worker sends telemetry through its serial queue, so it never overtakes the
  `call_started` it belongs to. While the app is not welcomed it drops the counts and holds the
  events in memory, up to 200 per session, and sends them with the next post after `welcome`.
- A message of another `schema` is dropped. The app warns once per session: mc-meet and the app
  then differ in version.
- Events, by `name`:

| `name` | From | Attributes |
| --- | --- | --- |
| `mc.caption.channel.opened` | page hook, on the channel's `open` | channel label, id, `serial`, `is_own`, `negotiated`, `ordered`, `max_retransmits` |
| `mc.caption.channel.closed` | page hook, on `close` or a closed `readyState` | channel attributes, `lifetime_ms` |
| `mc.caption.channel.error` | page hook | channel attributes, `error.type` (a WebRTC `errorDetail`, or `unknown`) |
| `mc.caption.channel.close_blocked` | page hook, when it blocks Meet's `close()` | as `opened` |
| `mc.caption.first_frame` | page hook, per channel | channel attributes, `age_ms` |
| `mc.caption.first_caption` | page hook, per channel | channel attributes, `age_ms` |
| `mc.caption.press` | content script, at each captions press | `kind` (`click` or `shortcut`), `reason` (`first` or `retry`), `has_focus`, `is_visible`, the control's `state_before` and `kind`, `number` |
| `mc.caption.control.changed` | content script, when the captions control changes | `state` and `state_before` (`on`, `off`, `unavailable`, `not_found`), `kind` (`button`, `menu`, `none`), `label`, `notice` |

- `label` codes the control's own label, and `notice` the last "captions unavailable" text Meet
  showed. Both use the English texts in `content/selectors.js`: `turn_on_captions`,
  `turn_off_captions`, `captions_temporarily_unavailable`, `captions_unavailable`,
  `captions_and_translation`, `none` for no text, and `unknown` for any other text.
- `notice` starts each call at `none`: the last call's text is forgotten.
- A press has one of two reasons: `first` for the call's first press, `retry` for each later one.
- mc-meet does not report whether the user speaks. Meet's speaking indicator has no stable
  attribute or role to read, only generated class names and an animation.
- The app writes the row to the call's `debug/<call>/metrics.jsonl` with `service.name`
  `mc-meet` and the call's `trace_id`. It logs each event at INFO with target `mc_meet`,
  `event.name`, `mc.meet.at` and the attributes as JSON in `mc.meet.attributes`. It also
  appends each event to the call's `debug/<call>/events.jsonl` with the same fields.
- Debug mode: the app sends `debug_mode` with its end time. The worker stores it in
  `chrome.storage.session` as `debugModeUntil`, removes the key when `until` is null or the
  native port closes, and tells each tab. Until then, the page hook also copies every caption
  channel frame, and the worker sends them as `telemetry_frames`. The content script sends none
  while the call is stopped. The app appends them to the call's `frames.jsonl` as `raw_frame`
  lines, up to 50 MB per call. It drops them when its own debug mode is off or the user stopped
  the call.

## Debug material

The app keeps debug material per call in `debug/<call>/` in the data folder. Raw frames hold call
speech, so it never leaves the data folder unless the user saves a report. The app deletes debug
folders after 14 days.

| File | What |
| --- | --- |
| `frames.jsonl` | The first 20 `frame_failed` messages of the call, one per line, bytes included. In debug mode also every caption frame, as `raw_frame` lines, up to 50 MB. |
| `metrics.jsonl` | mc-meet's metric rows, one per second. See "Telemetry". |
| `events.jsonl` | mc-meet's events as `mc-meet` log lines, stamped with mc-meet's time. See "Telemetry". |
| `lead-up.jsonl` | The last 2 minutes of mc-meet events before the call's first problem, written once. Kept in memory until then. |
| `page-snapshots.jsonl` | Up to 5 page snapshots per call, one per problem. |

A page snapshot holds the captions button state and the last "captions unavailable" text Meet
showed on it, the caption source policy's health, the caption region's element tree with each
text replaced by its length, the last 30 data channels the hook saw with their state, the frame
counters per channel, Meet's build label when the page exposes it, the Chrome version and the
mc-meet version. It holds no caption text.

"Save debug report" (tray, or popup) writes `debug-reports/<time>-<call>/` in the data folder:
`call-status.json`, the app log lines that name the call, the call's session logs and its debug
files, with a fresh dump of the app's flight recorder. By default the report keeps only strings
under keys known to hold times, ids, versions and codes. Every other string becomes `[N chars]`:
caption and chat text, speaker, sender and participant names, meeting titles and codes, Slack
channels, workspaces and window titles, error messages and `detail`. Device and message IDs become
`device-N` and `message-N`, the same number for the same ID across the report, since the DOM
reader uses a speaker's name as the device ID.
Frame bytes are dropped. "Save debug report with caption text" keeps everything.

`just replay-frames <path>` runs the frames of a report folder, a call's debug folder or a
`frames.jsonl` through `extension/page/wire.js` in Node, and prints what each frame decodes to
now. A report without caption text has no bytes to replay.

## App rules

- Log: `browser-calls/<started_at>-<session_id>.jsonl` in the data folder, `started_at` as
  `20261019T115918.000Z`. Each line is `{"seq":..,"received_at":..,"event":{..}}`, appended and
  flushed before the `ack`. The event is stored as received, unknown fields included.
- The call ends on `call_ended`, or 5 minutes after the last event once no port has the session
  open. Then intake reads the log as `InputKind::BrowserCallLog`. For the second case the app
  appends its own `call_ended` line, with reason `idle` and no `seq`.
- A new session joins the log of an earlier call when its `call_started` has the same meeting
  code, and that call is open or its last event is at most 10 minutes before. This covers
  leaving and joining again, and joining from a new tab after a crash. The note stays one note.
  - A `<session_id>.rejoin` file next to the logs names the log the session joined, so resends
    and a restart find it.
  - The call ends when its last session ends.
  - Two different meetings on one meeting code, such as a personal room, merge when the second
    starts within 10 minutes of the first.
- An event for a session whose log already ended reopens that log. The call ends again by the
  same rules, and intake updates the same meeting and note.
- A call with fewer than 3 caption lines writes no note, as for huddles. The log stays.
- At start, the app closes each log with no end line from a previous run and takes it in. The
  end line is a `call_ended` with reason `app_restarted` and no `seq`, at the time of the last
  event.
- Attendees are every participant in any `roster` event, the local user included. Speakers and
  chat senders join the list when no roster arrived.
- The tray shows whether mc-meet is connected, and one status line per open call. See
  `docs/02-architecture.md`, "Call status".

## Install

Chrome installs extensions from outside the Chrome Web Store only as an unpacked folder. The app
bundles mc-meet and keeps that folder for you:

- macOS: `~/Library/Application Support/com.ujjwalkumar.meetingcapture/mc-meet`
- Windows: `%APPDATA%\ujjwalkumar\meetingcapture\data\mc-meet`

At each start, the app checks the folder's manifest version. When it differs from the app's
version, the app copies its bundled extension there, stamped with its own version, and leaves out
the docs. The tray item below always copies it again, which repairs a folder with missing files.

To install:

1. In the tray, pick "Install mc-meet in Chrome…". The app shows the folder in Finder or Explorer
   and opens `chrome://extensions` in Chrome.
2. Turn on Developer mode.
3. Drag the `mc-meet` folder onto the page, or click "Load unpacked" and pick it.
4. Pin mc-meet from the extensions menu in the toolbar, so its call status stays in view.

For Brave, Edge or another Chromium browser, open its extensions page and load the same folder.

Updates need no steps. In `welcome`, the app sends the folder's version as `folder_version`. When
that differs from the extension's version, the worker waits until the last call ends and its port
closes, then calls `chrome.runtime.reload()`, so Chrome reads the new files. It reloads once per
folder version and stores that version in `reloaded_for`. A folder the app does not manage
therefore reloads once, not after every call.

From a clone, load the repo's `extension/` folder instead, and click its reload icon after a pull.

### Dev runs on macOS

- Run a dev app from outside `~/Documents`. Privacy controls stop Chrome from starting a host
  there, and Chrome reports only "Native host has exited".
- Chrome reads host manifests from its default config folder, even when started with a custom
  `--user-data-dir`. A dev app overwrites the installed app's manifest there.
- An extension loaded through CDP `Extensions.loadUnpacked` is gone after a browser restart. Its
  storage stays.
- A fake microphone built on an `AudioContext` starts suspended. Resume it from a user gesture,
  or the call gets no speech and no captions.
- The tray shows mc-meet as connected only while the worker has work, since the port closes
  after the last ack.
- A worker with queued events starts the app through host mode, and that app points the host
  manifests at itself. Uninstall the extension before stopping a dev app, or a later call reaches
  the dev build.

## Sources

- DOM readers: TranscripTonic, MIT, <https://github.com/vivek-nexus/transcriptonic>, commit
  `0cb5eb5667759cd9cd0bb98d1ce6bdb763a30151`. The licence is in `extension/content/LICENSE-transcriptonic`.
- Host relay: Anarlog's `crates/chrome-native-host`, MIT, <https://github.com/fastrepl/anarlog>,
  commit `6fd155aa1af39ef652b767133caccb280de750f6`.
- Data channel layout: our own decoder. Field numbers and their sources are in
  `extension/PROTOCOL-NOTES.md`.
