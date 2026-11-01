// Builds the native telemetry messages from a tab's once-a-second telemetry post. It makes no
// chrome.* calls and reads no clock, so it also runs in Node.
//
// A channel's counts are cumulative from its start. The app builds the metric row from them, so
// the message carries counts and codes only, never a point key.

export const TELEMETRY_SCHEMA = 2;

function iso(atMs) {
  return new Date(atMs).toISOString();
}

function channelCounts(channel) {
  return {
    label: channel.label,
    id: channel.id,
    serial: channel.serial,
    is_own: channel.is_own,
    started_at: iso(channel.started_at_ms),
    raw: channel.raw,
    parsed: channel.parsed,
    control: channel.control,
    rejected: channel.rejected,
    acks: channel.acks,
  };
}

// The `telemetry` message for one tab post, or null when it holds nothing. events are the ones
// to send, which may include events held from earlier posts.
export function telemetryMessage(post, events) {
  const channels = Array.isArray(post.channels) ? post.channels : [];
  if (channels.length === 0 && events.length === 0) return null;
  return {
    type: "telemetry",
    schema: TELEMETRY_SCHEMA,
    session_id: post.session_id,
    at: iso(post.at_ms),
    channels: channels.map(channelCounts),
    events: events.map((event) => ({
      at: iso(event.at_ms),
      name: event.name,
      attributes: event.attributes ?? {},
    })),
  };
}

// The `telemetry_frames` message for one tab post, or null when it holds no frame.
export function framesMessage(post) {
  const frames = Array.isArray(post.frames) ? post.frames : [];
  if (frames.length === 0) return null;
  return {
    type: "telemetry_frames",
    session_id: post.session_id,
    frames: frames.map((frame) => ({
      channel: frame.channel,
      channel_id: frame.channel_id,
      is_own: frame.is_own,
      at: iso(frame.at_ms),
      byte_length: frame.byte_length,
      bytes_base64: frame.bytes_base64,
    })),
  };
}

// Whether the app's debug mode runs at nowMs. until is the RFC 3339 end the app sent, or absent.
export function isDebugModeOn(until, nowMs) {
  if (typeof until !== "string") return false;
  const untilMs = Date.parse(until);
  return Number.isFinite(untilMs) && untilMs > nowMs;
}
