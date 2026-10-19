// Call events in the wire shapes of docs/09-meet-extension.md. Every event the extension sends
// is built here, so the schema has one owner. A plain script with no imports or exports: the
// manifest loads it before page/wire.js in the MAIN world and before the content scripts in the
// isolated world, and background.js imports it for its side effect.
// Times go in as milliseconds and out as RFC 3339 UTC strings.
globalThis.mcMeetCallEvents = Object.freeze({
  // The worker fills in tab_id: a content script cannot read it.
  callStarted: ({ atMs, meetingCode, title }) => ({
    kind: "call_started",
    at: new Date(atMs).toISOString(),
    platform: "meet",
    meeting_code: meetingCode,
    title,
    tab_id: null,
  }),

  callEnded: ({ atMs, reason }) => ({
    kind: "call_ended",
    at: new Date(atMs).toISOString(),
    reason,
  }),

  caption: ({ atMs, deviceId, utteranceId, version, speaker, text, source }) => ({
    kind: "caption",
    at: new Date(atMs).toISOString(),
    device_id: deviceId,
    utterance_id: utteranceId,
    version,
    speaker,
    text,
    source,
  }),

  // deviceId is undefined for a message with no device, and JSON then leaves device_id out.
  chat: ({ atMs, messageId, deviceId, sender, text, source }) => ({
    kind: "chat",
    at: new Date(atMs).toISOString(),
    message_id: messageId,
    device_id: deviceId,
    sender,
    text,
    source,
  }),

  rosterParticipant: ({ deviceId, name, isSelf }) => ({
    device_id: deviceId,
    name,
    is_self: isSelf,
  }),

  // participants come from rosterParticipant.
  roster: ({ atMs, participants }) => ({
    kind: "roster",
    at: new Date(atMs).toISOString(),
    participants,
  }),

  health: ({ atMs, source, captionsOn, parseFailures, detail }) => ({
    kind: "health",
    at: new Date(atMs).toISOString(),
    source,
    captions_on: captionsOn,
    parse_failures: parseFailures,
    detail,
  }),
});
