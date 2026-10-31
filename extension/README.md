# mc-meet

Chrome extension for Google Meet. It streams captions, chat, participants and call start and end
to the meeting-capture app over native messaging. The design is in `docs/09-meet-extension.md`.

## Files

| File | Runs in | Job |
| --- | --- | --- |
| `manifest.json` | | MV3 manifest. Its `key` pins the ID `pbpjeihgfeefjehjnjknnefomhkblcfk`. |
| `call-events.js` | Meet tab isolated world and the service worker | Builds each call event in its wire shape. |
| `page/wire.js` | Meet page, MAIN world | Protobuf decoder and Meet frame reader. |
| `page-hook.js` | Meet page, MAIN world | Wraps `RTCPeerConnection` and `fetch`, opens the caption channels. |
| `content/selectors.js` | Meet tab, isolated world | Every DOM selector, in one table. |
| `content/dom-readers.js` | Meet tab, isolated world | DOM caption and participant fallbacks. Adapted from TranscripTonic. |
| `content/caption-source.js` | Meet tab, isolated world, or plain Node | Caption source policy: picks the channel or the DOM reader, and owns the health fields. No DOM. |
| `content/main.js` | Meet tab, isolated world | Call start and end, session, runs the DOM reader the policy picks, sends health events. |
| `session-rules.js` | Service worker, or plain Node | Session rules and event numbers, with no `chrome.*` calls. |
| `background.js` | Service worker | Storage, native port, queue, resend. Runs the effects `session-rules.js` returns. Sets each Meet tab's toolbar icon from the app's call status. |
| `tab-video.js` | Service worker | Picks the Meet tab whose call wants video, starts and stops its capture, forwards fragments to the app. |
| `offscreen/offscreen.html` | Offscreen document | Page that loads `capture.js`. |
| `offscreen/capture.js` | Offscreen document | Opens the tab stream, plays its audio back, encodes video with WebCodecs, sends fMP4 fragments. |
| `offscreen/fmp4.js` | Offscreen document, or plain Node | fMP4 init segment and fragment writer for one video track. No DOM. |
| `popup/` | Toolbar popup | The active tab's call status, its video state and a "Save debug report" button. |
| `icons/` | | Toolbar icons: idle, recording, problem and off, at 16 and 32 px. |
| `PROTOCOL-NOTES.md` | | Field numbers the decoder relies on, with sources. |

## Load it unpacked

A released app installs its own copy and updates it. See `docs/09-meet-extension.md`. These steps
load the clone's folder for development.

1. Open `chrome://extensions`. Turn on Developer mode.
2. Click "Load unpacked". Pick this `extension/` folder.
3. Check that the card shows ID `pbpjeihgfeefjehjnjknnefomhkblcfk`. The app's host manifest allows
   only that ID.
4. Start the meeting-capture app once, so it writes the native host manifest.

After a pull, click the reload icon on the mc-meet card. Reload any open Meet tab if the extension
was not running when the tab opened.

## First use

1. Join a Meet call. Do not turn captions on.
2. On the mc-meet card, click "service worker" to open its console.
   - A `mc-meet: app <version> open sessions [...]` line means the app answered `hello`.
   - `mc-meet: native port closed: Specified native messaging host not found.` means the app has not
     written its host manifest. Start the app.
   - `mc-meet: the app did not start` means host mode gave up after 10 s.
3. In the app's session log (`browser-calls/<started_at>-<session_id>.jsonl`), check:
   - one `call_started` line;
   - a `health` line with `"source":"channel"` once someone speaks. `"source":"dom"` means the
     channels gave no caption in 20 s, and `detail` says why;
   - every `health` line has `"caption_box_shown":false`: Meet's caption box never shows.
     `"captions_on"` turns true about 30 s in: mc-meet pressed Meet's captions button once to
     start the stream, and the button reads on with no box;
   - `roster` lines with your name and `"is_self":true`;
   - `caption` lines whose `version` grows for one `utterance_id`.
4. Leave the call. A `call_ended` line with `"reason":"left"` follows within about 4 s.

A `health` event with a `skeleton` in `detail` means Meet changed a frame format. Compare the
skeleton with `PROTOCOL-NOTES.md`. "Save debug report with caption text" in the tray keeps the
failing frames; `just replay-frames <report folder>` runs them through `page/wire.js`.

## Rules

- No network requests, telemetry, remote config or downloads. The popup shows what the app
  reports and talks only to the service worker.
- The extension never changes the caption language, and presses Meet's captions
  button at most once per call: a second press would stop the caption stream.
- Plain JavaScript, no build step, no npm packages. `biome check extension/` must pass.

## Licences

`content/dom-readers.js` adapts TranscripTonic, MIT, <https://github.com/vivek-nexus/transcriptonic>,
commit `0cb5eb5667759cd9cd0bb98d1ce6bdb763a30151`. Its licence is in
`content/LICENSE-transcriptonic`.
