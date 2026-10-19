# Meet data channel notes

`page/wire.js` decodes Meet's protobuf frames by field number. Google publishes no schema. The
numbers below come from reading other extensions' shipped code. No code was copied from them.

Sources:

- Tactiq 3.1.7185, its `googlemeet.inline.js` (proprietary; read for facts only). Cited as
  `T:<line>`, a line in the file after Prettier.
- Attendee, its `meet_payload.js` (Elastic License 2.0; read for facts only). Cited as
  `A:<line>`.
- Fireflies notes in `~/knowledge/areas/infrastructure/meeting-recording/local-capture/how-tools-work/meet-extensions.md`.
- Frames captured in a live two-account call on 2026-10-19. Cited as "live frame".

## Frames

Some frames are gzipped. A frame is inflated when it starts with `1f 8b 08`, or has those bytes at
offset 3 (legacy captions). Source: T:10409-10413.

## Field numbers

| Message | Path or field | Meaning | Source |
| --- | --- | --- | --- |
| `captions_v2` frame | `1` | utterance | T:9038 (`CaptionsV2Packet`), A:1724 |
| utterance | `1` varint | utterance ID (int64, may exceed 2^53) | T:9109, A:1734 |
| utterance | `2` varint | version, grows as Meet revises the text | T:9109, A:1735 |
| utterance | `3` | caption | T:9109, A:1736 |
| caption | `3` string | text | T:9259, A:1746 |
| caption | `4` string | language code | T:9259, A:1750 |
| caption | `6` string | device ID, `spaces/<id>/devices/<id>` | T:9259, A:1753 |
| `captions_v2` ack (we send) | `1.1.{1,2,3}` | utterance ID, version, constant 1 | T:9348-9530, T:9772 (`Yu`) |
| `captions` frame (legacy) | `1` | caption; field `2` present means a control frame | T:7192, T:9643 (`ju`) |
| legacy caption | `1` string, `2` varint, `3` varint, `6` string, `8` varint | device ID, utterance ID, version, text, language ID | T:7009, A:1772-1779 |
| `collections` frame | `1.2.13` | body | T:7276-7540 (`BDevice` nesting), A:1628-1653 |
| body | `1.2` repeated | participant | T:7474 (`BDeviceSub3/4`), A:1700-1705 |
| body | `4` repeated, then `2` | chat message | T:6649-6943 (`BChatMessageSub*`), A:1652 |
| participant | `1` string | device ID | T:7540 (`BDeviceSub5`), A:1711 |
| participant | `2` string | full name | T:7540, A:1712 |
| participant | `4` varint | status: 1 in call, 4 refused (unsure), 6 left, 7 removed | A:1714, Fireflies note |
| participant | `7` string | present only on the local user | A:1715 |
| participant | `21` string | parent device: this device is a screen share | A:1717 |
| participant | `29` string | display name, used when `2` is empty | A:1716 |
| chat message | `1` string | message name, `spaces/<id>/messages/<id>` | A:1791, T:10024 (`ji` byte search) |
| chat message | `2` string | sender device ID | T:6526, A:1792 |
| chat message | `3` varint | the sender's join time, milliseconds since the epoch | T:6526, A:1793, live frame |
| chat message | `4.1` varint, `4.2` varint | sent time: seconds since the epoch, then nanos | live frame |
| chat message | `5.1` string | text | T:6471, T:6526, A:1794-1800 |
| `meet_messages` frame | collections chat nesting, or a bare chat message | chat | T:9623 (`Bu`), T:10808 |
| `SyncMeetingSpaceCollections` response | base64 text, then `2.2.2` repeated | participant | T:7684-7899 (`BMeetingCollection`), T:10971, A:1896-1908 |
| `CreateMeetingMessage` response | base64 text, then a chat message | own sent chat | T:10986 |
| `CreateMeetingDevice` response, `SyncMeetingSpaceCollections` request | first `spaces/<id>/devices/<id>` | local device, used when no participant has field `7` | T:10879-10886, T:10926-10935 |

RPC URLs are under `https://meet.google.com/$rpc/google.rtc.meetings.v1.` (T:10286-10300):
`MeetingSpaceService/SyncMeetingSpaceCollections`, `MeetingMessageService/CreateMeetingMessage` and
`MeetingDeviceService/CreateMeetingDevice`.

## Opening caption channels

The hook calls `createDataChannel("captions" | "captions_v2", {ordered: true, maxRetransmits: 10})`
on the peer connection that carries `collections`. Tactiq does the same and states that Meet then
streams captions without the captions button (T:10534, T:10714-10724). It acks `captions_v2`
frames only on channels it opened; Meet acks its own (T:10668-10671).

## Not used

- `media-session` caption language config (T:9697-9727). We never change the caption language.
- Device output and stream info on `collections` (`1.2.3`, A:1644-1668).

## When a format changes

`health.detail` carries a skeleton of a failing frame after 10 failures in a row on one channel:
the tag and wire type tree with lengths, three levels deep, no text. Compare it with the table.
