# mc-meet

Chrome extension for Google Meet. It streams captions, chat, participants and call start and end
to the meeting-capture app over native messaging. The design is in `docs/09-meet-extension.md`.

## Files

| File | Runs in | Job |
| --- | --- | --- |
| `manifest.json` | | MV3 manifest. Its `key` pins the ID `pbpjeihgfeefjehjnjknnefomhkblcfk`. |
| `call-events.js` | Both Meet tab worlds and the service worker | Builds each call event in its wire shape. |
| `page/wire.js` | Meet page, MAIN world | Protobuf decoder and Meet frame reader. |
| `page-hook.js` | Meet page, MAIN world | Wraps `RTCPeerConnection` and `fetch`, opens the caption channels. |
| `content/selectors.js` | Meet tab, isolated world | Every DOM selector, in one table. |
| `content/dom-readers.js` | Meet tab, isolated world | DOM caption and participant fallbacks. Adapted from TranscripTonic. |
| `content/main.js` | Meet tab, isolated world | Call start and end, session, caption source, health. |
| `session-rules.js` | Service worker, or plain Node | Session rules and event numbers, with no `chrome.*` calls. |
| `background.js` | Service worker | Storage, native port, queue, resend. Runs the effects `session-rules.js` returns. |
| `PROTOCOL-NOTES.md` | | Field numbers the decoder relies on, with sources. |

## Load it unpacked

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
   - `roster` lines with your name and `"is_self":true`;
   - `caption` lines whose `version` grows for one `utterance_id`.
4. Leave the call. A `call_ended` line with `"reason":"left"` follows within about 4 s.

A `health` event with a `skeleton` in `detail` means Meet changed a frame format. Compare the
skeleton with `PROTOCOL-NOTES.md`.

## Rules

- No network requests, telemetry, remote config, popup or downloads.
- The extension never changes the caption language. It clicks the captions button only when it
  falls back to the DOM and captions are off.
- Plain JavaScript, no build step, no npm packages. `biome check extension/` must pass.

## Licences

`content/dom-readers.js` adapts TranscripTonic, MIT, <https://github.com/vivek-nexus/transcriptonic>,
commit `0cb5eb5667759cd9cd0bb98d1ce6bdb763a30151`. Its licence is in
`content/LICENSE-transcriptonic`.
