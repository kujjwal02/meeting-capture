// Protobuf wire decoder and Meet frame reader. Runs in the Meet page (MAIN world) before
// page-hook.js, and in Node for the fixture script. It touches no DOM and no network.
// It reports readings with camelCase fields; the content script turns them into call events,
// since Chrome injects call-events.js into one world of a tab only.
// Field numbers and their sources are in extension/PROTOCOL-NOTES.md.
(() => {
  if (globalThis.mcMeetWire) return;

  class WireError extends Error {}

  // Protobuf wire types we accept.
  const VARINT = 0;
  const I64 = 1;
  const LEN = 2;
  const I32 = 5;

  function readVarint(bytes, start) {
    let value = 0n;
    let shift = 0n;
    let pos = start;
    for (let i = 0; i < 10; i++) {
      if (pos >= bytes.length) throw new WireError("varint runs past the end");
      const byte = bytes[pos++];
      value |= BigInt(byte & 0x7f) << shift;
      if ((byte & 0x80) === 0) return { value, next: pos };
      shift += 7n;
    }
    throw new WireError("varint longer than 10 bytes");
  }

  // Parses one message level into a Map of field number to a list of raw values.
  // Each value is { wireType, varint } or { wireType, bytes }.
  function parseFields(bytes) {
    const fields = new Map();
    let pos = 0;
    while (pos < bytes.length) {
      const key = readVarint(bytes, pos);
      pos = key.next;
      const fieldNumber = Number(key.value >> 3n);
      const wireType = Number(key.value & 7n);
      if (fieldNumber === 0) throw new WireError("field number 0");
      let value;
      if (wireType === VARINT) {
        const varint = readVarint(bytes, pos);
        pos = varint.next;
        value = { wireType, varint: varint.value };
      } else if (wireType === I64 || wireType === I32) {
        const size = wireType === I64 ? 8 : 4;
        if (pos + size > bytes.length) throw new WireError("fixed field runs past the end");
        value = { wireType, bytes: bytes.subarray(pos, pos + size) };
        pos += size;
      } else if (wireType === LEN) {
        const length = readVarint(bytes, pos);
        pos = length.next;
        const size = Number(length.value);
        if (pos + size > bytes.length)
          throw new WireError("length-delimited field runs past the end");
        value = { wireType, bytes: bytes.subarray(pos, pos + size) };
        pos += size;
      } else {
        throw new WireError(`unsupported wire type ${wireType}`);
      }
      const list = fields.get(fieldNumber);
      if (list) list.push(value);
      else fields.set(fieldNumber, [value]);
    }
    return fields;
  }

  const utf8 = new TextDecoder("utf-8", { fatal: true });

  function lenValues(fields, fieldNumber) {
    return (fields.get(fieldNumber) ?? []).filter((v) => v.wireType === LEN);
  }

  function lastLen(fields, fieldNumber) {
    const values = lenValues(fields, fieldNumber);
    return values.length ? values[values.length - 1].bytes : undefined;
  }

  function stringField(fields, fieldNumber) {
    const bytes = lastLen(fields, fieldNumber);
    return bytes === undefined ? undefined : utf8.decode(bytes);
  }

  function varintField(fields, fieldNumber) {
    const values = (fields.get(fieldNumber) ?? []).filter((v) => v.wireType === VARINT);
    return values.length ? values[values.length - 1].varint : undefined;
  }

  function messageField(fields, fieldNumber) {
    const bytes = lastLen(fields, fieldNumber);
    return bytes === undefined ? undefined : parseFields(bytes);
  }

  function repeatedMessages(fields, fieldNumber) {
    return lenValues(fields, fieldNumber).map((v) => parseFields(v.bytes));
  }

  // Follows single nested messages by field number. Returns undefined when a level is absent.
  function descend(fields, path) {
    let current = fields;
    for (const fieldNumber of path) {
      current = messageField(current, fieldNumber);
      if (!current) return undefined;
    }
    return current;
  }

  function isGzipAt(bytes, offset) {
    return (
      bytes.length >= offset + 3 &&
      bytes[offset] === 0x1f &&
      bytes[offset + 1] === 0x8b &&
      bytes[offset + 2] === 0x08
    );
  }

  async function gunzip(bytes) {
    const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("gzip"));
    return new Uint8Array(await new Response(stream).arrayBuffer());
  }

  // Meet gzips some frames. Legacy caption frames can carry a 3-byte prefix before the gzip header.
  async function inflateFrame(bytes) {
    if (isGzipAt(bytes, 0)) return gunzip(bytes);
    if (isGzipAt(bytes, 3)) return gunzip(bytes.subarray(3));
    return bytes;
  }

  // ---- Encoder, only for the captions_v2 ack.

  function varintBytes(value) {
    const out = [];
    let rest = BigInt(value);
    do {
      let byte = Number(rest & 0x7fn);
      rest >>= 7n;
      if (rest > 0n) byte |= 0x80;
      out.push(byte);
    } while (rest > 0n);
    return out;
  }

  function varintEntry(fieldNumber, value) {
    return [...varintBytes((fieldNumber << 3) | VARINT), ...varintBytes(value)];
  }

  function lenEntry(fieldNumber, body) {
    return [...varintBytes((fieldNumber << 3) | LEN), ...varintBytes(body.length), ...body];
  }

  // CaptionsV2Ack { 1: { 1: { 1: utterance id, 2: version, 3: 1 } } }
  function encodeCaptionV2Ack(utteranceId, version) {
    const ref = [...varintEntry(1, utteranceId), ...varintEntry(2, version), ...varintEntry(3, 1n)];
    return new Uint8Array(lenEntry(1, lenEntry(1, ref)));
  }

  // ---- Skeleton: the tag and wire type tree of a frame, with lengths but no content.

  const SKELETON_DEPTH = 3;
  const SKELETON_CHARS = 240;
  const WIRE_NAMES = { 0: "VARINT", 1: "I64", 5: "I32" };

  function skeletonOf(bytes, depth) {
    const fields = parseFields(bytes);
    const parts = [];
    for (const [fieldNumber, values] of fields) {
      for (const value of values) {
        if (value.wireType !== LEN) {
          parts.push(`${fieldNumber}:${WIRE_NAMES[value.wireType]}`);
          continue;
        }
        let children = "";
        if (depth < SKELETON_DEPTH && value.bytes.length > 0) {
          try {
            children = `{${skeletonOf(value.bytes, depth + 1)}}`;
          } catch {
            children = "";
          }
        }
        parts.push(`${fieldNumber}:LEN(${value.bytes.length})${children}`);
      }
    }
    return parts.join(" ");
  }

  function skeleton(bytes) {
    if (bytes.length === 0) return "(empty)";
    let text;
    try {
      text = skeletonOf(bytes, 0);
    } catch {
      text = `not-protobuf(${bytes.length})`;
    }
    return text.length > SKELETON_CHARS ? `${text.slice(0, SKELETON_CHARS - 3)}...` : text;
  }

  // ---- Meet messages. Each decoder returns { ok: true, ... } or { ok: false, reason }.

  // captions_v2: CaptionsV2Packet { 1: utterance { 1: id, 2: version, 3: caption { 3: text,
  // 4: language, 6: device id } } }. Meet also wraps utterances one level deeper, at 1.1, next to
  // status entries that hold no caption. An utterance's field 1 is a varint id, a wrapper's is a
  // message, so field 3 tells them apart. A wrapper with fields but no utterance is a control
  // frame: Meet sends one each second while the channel is open.
  function decodeCaptionV2(bytes) {
    let utterances;
    try {
      const first = messageField(parseFields(bytes), 1);
      if (!first) return { ok: false, reason: "no-utterance" };
      if (first.has(3)) {
        utterances = [first];
      } else {
        utterances = repeatedMessages(first, 1).filter((entry) => entry.has(3));
        if (utterances.length === 0 && first.size > 0) {
          return { ok: false, control: true, reason: "control" };
        }
      }
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
    if (utterances.length === 0) return { ok: false, reason: "no-utterance" };
    // One bad utterance must not drop the good ones next to it. The frame fails only when none
    // decode, and then with the first reason.
    const decoded = utterances.map(decodeUtteranceV2);
    const captions = decoded.filter((caption) => caption.ok);
    return captions.length > 0 ? { ok: true, captions } : decoded[0];
  }

  function decodeUtteranceV2(utterance) {
    try {
      const caption = messageField(utterance, 3);
      const deviceId = caption ? stringField(caption, 6) : undefined;
      if (!deviceId) return { ok: false, reason: "no-device" };
      const utteranceId = varintField(utterance, 1);
      if (!utteranceId) return { ok: false, reason: "no-utterance-id" };
      return {
        ok: true,
        deviceId,
        utteranceId,
        version: varintField(utterance, 2) ?? 0n,
        text: stringField(caption, 3) ?? "",
        languageCode: stringField(caption, 4) ?? "",
      };
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
  }

  // captions (legacy): { 1: caption { 1: device id, 2: id, 3: version, 6: text, 8: language id },
  // 2: control marker }. A frame with field 2 is a control frame, not a caption.
  function decodeCaptionV1(bytes) {
    let fields;
    try {
      fields = parseFields(bytes);
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
    if (fields.has(2)) return { ok: false, control: true, reason: "control" };
    try {
      const caption = messageField(fields, 1);
      if (!caption) return { ok: false, reason: "no-caption" };
      const deviceId = stringField(caption, 1);
      if (!deviceId) return { ok: false, reason: "no-device" };
      const utteranceId = varintField(caption, 2);
      if (!utteranceId) return { ok: false, reason: "no-utterance-id" };
      return {
        ok: true,
        deviceId,
        utteranceId,
        version: varintField(caption, 3) ?? 0n,
        text: stringField(caption, 6) ?? "",
        languageCode: "",
      };
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
  }

  // Participant record, from the collections channel and SyncMeetingSpaceCollections:
  // { 1: device id, 2: full name, 4: status, 7: present only for the local user,
  //   21: parent device id (screen share), 29: display name }
  function decodeParticipant(fields) {
    const deviceId = stringField(fields, 1);
    if (!deviceId) return undefined;
    const status = varintField(fields, 4);
    return {
      deviceId,
      name: stringField(fields, 2) || stringField(fields, 29) || "",
      status: status === undefined ? undefined : Number(status),
      isSelf: fields.has(7),
      parentDeviceId: stringField(fields, 21),
    };
  }

  // Chat message: { 1: message name, 2: device id, 3: the sender's join time in ms,
  // 4: sent time { 1: seconds, 2: nanos }, 5: { 1: text } }
  function decodeChatMessage(fields) {
    const deviceId = stringField(fields, 2);
    const body = messageField(fields, 5);
    const text = body ? stringField(body, 1) : undefined;
    if (!deviceId || !text) return undefined;
    const sentMs = timestampMs(messageField(fields, 4));
    return {
      messageId: stringField(fields, 1) || `${deviceId}/${sentMs ?? ""}`,
      deviceId,
      sentMs,
      text,
    };
  }

  function timestampMs(timestamp) {
    const seconds = timestamp ? varintField(timestamp, 1) : undefined;
    if (seconds === undefined) return undefined;
    const nanos = varintField(timestamp, 2) ?? 0n;
    return Number(seconds * 1000n + nanos / 1000000n);
  }

  // collections: { 1: { 2: { 13: { 1: { 2: repeated participant }, 4: repeated { 2: chat } } } } }
  function decodeCollections(bytes) {
    let inner;
    try {
      const fields = parseFields(bytes);
      if (!fields.has(1)) return { ok: false, reason: "no-body" };
      inner = descend(fields, [1, 2, 13]);
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
    if (!inner) return { ok: true, participants: [], chats: [] };
    try {
      const list = messageField(inner, 1);
      const participants = list
        ? repeatedMessages(list, 2)
            .map(decodeParticipant)
            .filter((p) => p !== undefined)
        : [];
      const chats = repeatedMessages(inner, 4)
        .map((wrapper) => messageField(wrapper, 2))
        .filter((m) => m !== undefined)
        .map(decodeChatMessage)
        .filter((c) => c !== undefined);
      return { ok: true, participants, chats };
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
  }

  // meet_messages: the collections chat nesting, or a bare chat message.
  function decodeMeetMessage(bytes) {
    const nested = decodeCollections(bytes);
    if (nested.ok && nested.chats.length) return { ok: true, chats: nested.chats };
    try {
      const chat = decodeChatMessage(parseFields(bytes));
      if (chat) return { ok: true, chats: [chat] };
      return { ok: false, reason: "no-chat" };
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
  }

  // SyncMeetingSpaceCollections response body, base64 decoded:
  // { 2: { 2: { 2: repeated participant } } }
  function decodeSyncCollections(bytes) {
    try {
      const list = descend(parseFields(bytes), [2, 2]);
      if (!list) return { ok: false, reason: "no-list" };
      const participants = repeatedMessages(list, 2)
        .map(decodeParticipant)
        .filter((p) => p !== undefined);
      return { ok: true, participants };
    } catch {
      return { ok: false, reason: "decode-threw" };
    }
  }

  function base64ToBytes(text) {
    const binary = atob(text.trim());
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  }

  const DEVICE_NAME_PATTERN = /spaces\/[A-Za-z0-9_-]+\/devices\/[A-Za-z0-9_-]+/;

  function findDeviceName(bytesOrText) {
    const text =
      typeof bytesOrText === "string" ? bytesOrText : new TextDecoder().decode(bytesOrText);
    return text.match(DEVICE_NAME_PATTERN)?.[0];
  }

  // ---- Meeting reader: frames in, spec-shaped events out.

  // Device status values seen by other tools: 1 in the call, 4 refused from the lobby,
  // 6 left, 7 removed. Absent or other values count as present.
  const ABSENT_STATUSES = new Set([4, 6, 7]);
  const REMOVED_STATUS = 7;
  const PARSE_GAP_FIRST = 10;
  const PARSE_GAP_REPEAT = 100;
  // Failed frames reported with their bytes, per channel and call. The app keeps 20 per call.
  const FAILED_FRAMES_REPORTED = 20;
  // A larger frame is reported without its bytes: a native message to the app stays under 1 MB.
  const MAX_REPORTED_FRAME_BYTES = 256 * 1024;
  const MAX_UTTERANCES = 5000;
  const MAX_CHATS = 5000;

  // A sent time more than a day from the arrival time is a misread field.
  function chatTime(sentMs, arrivedMs) {
    if (sentMs === undefined || Math.abs(sentMs - arrivedMs) >= 86400000) return arrivedMs;
    return sentMs;
  }

  function newCounters() {
    return { raw: 0, parsed: 0, control: 0, rejected: 0, consecutive: 0, reported: 0 };
  }

  function bytesToBase64(bytes) {
    let binary = "";
    const chunk = 0x8000;
    for (let i = 0; i < bytes.length; i += chunk) {
      binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
    }
    return btoa(binary);
  }

  // emit(reading) receives caption, chat and roster readings: the fields of
  // call-events.js's builder of the same kind, plus kind. notice(notice)
  // receives { kind: "parse_gap" | "self_removed" | "frame_failed" } notices for the content
  // script. Only frame_failed of a caption channel reaches the app, as debug material outside the
  // call log.
  function createMeetReader({ now = () => Date.now(), emit, notice = () => {} }) {
    const devices = new Map();
    const utterances = new Map();
    const chatIds = new Set();
    const counters = {
      captions: newCounters(),
      captions_v2: newCounters(),
      collections: newCounters(),
      meet_messages: newCounters(),
    };
    const reasons = new Map();
    let selfDeviceId;
    let selfRemoved = false;
    let lastRosterKey = "";
    let lastRoster;
    let captionsV2Seen = false;

    const nameOf = (deviceId) => devices.get(deviceId)?.name ?? "";

    function parseFailures() {
      return Object.values(counters).reduce((sum, c) => sum + c.rejected, 0);
    }

    // bytes is the frame as decoded; rawBytes as received, before gzip inflation. Returns the
    // frame's outcome: "parsed", "control" or "rejected".
    function noteResult(label, result, bytes, rawBytes) {
      const counter = counters[label];
      counter.raw += 1;
      if (result.ok) {
        counter.parsed += 1;
        counter.consecutive = 0;
        return "parsed";
      }
      if (result.control) {
        counter.control += 1;
        return "control";
      }
      counter.rejected += 1;
      counter.consecutive += 1;
      reasons.set(result.reason, (reasons.get(result.reason) ?? 0) + 1);
      if (counter.reported < FAILED_FRAMES_REPORTED) {
        counter.reported += 1;
        notice({
          kind: "frame_failed",
          channel: label,
          at_ms: now(),
          reason: result.reason,
          skeleton: skeleton(bytes),
          byte_length: rawBytes.length,
          bytes_base64:
            rawBytes.length <= MAX_REPORTED_FRAME_BYTES ? bytesToBase64(rawBytes) : null,
        });
      }
      const run = counter.consecutive;
      if (run === PARSE_GAP_FIRST || (run > PARSE_GAP_FIRST && run % PARSE_GAP_REPEAT === 0)) {
        const reasonList = [...reasons.entries()].map(([r, n]) => `${r}:${n}`).join(",");
        notice({
          kind: "parse_gap",
          channel: label,
          consecutive: run,
          parse_failures: parseFailures(),
          detail: `${label}: ${run} frames in a row failed (${reasonList}); skeleton ${skeleton(bytes)}`,
        });
      }
      return "rejected";
    }

    function emitRosterIfChanged() {
      const participants = [...devices.entries()]
        .filter(([, d]) => !d.parentDeviceId && d.name && !ABSENT_STATUSES.has(d.status))
        .map(([deviceId, d]) => ({ deviceId, name: d.name, isSelf: deviceId === selfDeviceId }))
        .sort((a, b) => (a.deviceId < b.deviceId ? -1 : a.deviceId > b.deviceId ? 1 : 0));
      if (participants.length === 0) return;
      const key = JSON.stringify(participants);
      if (key === lastRosterKey) return;
      lastRosterKey = key;
      lastRoster = { kind: "roster", atMs: now(), participants };
      emit(lastRoster);
    }

    function mergeParticipants(participants) {
      for (const p of participants) {
        const known = devices.get(p.deviceId) ?? {};
        devices.set(p.deviceId, {
          name: p.name || known.name || "",
          status: p.status ?? known.status,
          parentDeviceId: p.parentDeviceId ?? known.parentDeviceId,
        });
        if (p.isSelf) selfDeviceId = p.deviceId;
        if (p.deviceId === selfDeviceId && p.status === REMOVED_STATUS && !selfRemoved) {
          selfRemoved = true;
          notice({ kind: "self_removed", at: new Date(now()).toISOString() });
        }
      }
      emitRosterIfChanged();
    }

    function emitChats(chats) {
      const arrivedMs = now();
      for (const chat of chats) {
        if (chatIds.has(chat.messageId)) continue;
        chatIds.add(chat.messageId);
        if (chatIds.size > MAX_CHATS) chatIds.delete(chatIds.values().next().value);
        emit({
          kind: "chat",
          atMs: chatTime(chat.sentMs, arrivedMs),
          messageId: chat.messageId,
          deviceId: chat.deviceId,
          sender: nameOf(chat.deviceId),
          text: chat.text,
          source: "channel",
        });
      }
    }

    function emitCaption(label, caption) {
      if (label === "captions_v2") captionsV2Seen = true;
      // Legacy and v2 frames number utterances differently. Once v2 works, v1 is a duplicate.
      else if (captionsV2Seen) return;
      if (!caption.text) return;
      const utteranceId =
        label === "captions" ? `v1-${caption.utteranceId}` : `${caption.utteranceId}`;
      const key = `${caption.deviceId}|${utteranceId}`;
      const version = Number(caption.version);
      const known = utterances.get(key);
      if (
        known &&
        (version < known.version || (version === known.version && caption.text === known.text))
      ) {
        return;
      }
      const firstAtMs = known?.firstAtMs ?? now();
      utterances.set(key, { version, text: caption.text, firstAtMs });
      if (utterances.size > MAX_UTTERANCES) utterances.delete(utterances.keys().next().value);
      emit({
        kind: "caption",
        atMs: firstAtMs,
        deviceId: caption.deviceId,
        utteranceId,
        version,
        speaker: nameOf(caption.deviceId),
        text: caption.text,
        source: "channel",
      });
    }

    // Decodes one data channel frame. Returns { acks, outcome, captions }: acks, one per
    // captions_v2 utterance, so the hook can ack frames on channels it opened itself; outcome,
    // "parsed", "control" or "rejected"; captions, how many captions with text the frame held.
    async function frame(label, data) {
      if (!counters[label]) return {};
      let bytes;
      try {
        bytes = await inflateFrame(data);
      } catch {
        const outcome = noteResult(label, { ok: false, reason: "gunzip-threw" }, data, data);
        return { outcome, captions: 0 };
      }
      if (label === "captions_v2") {
        const result = decodeCaptionV2(bytes);
        const outcome = noteResult(label, result, bytes, data);
        if (!result.ok) return { outcome, captions: 0 };
        for (const caption of result.captions) emitCaption(label, caption);
        return {
          outcome,
          captions: result.captions.filter((caption) => caption.text).length,
          acks: result.captions.map((caption) =>
            encodeCaptionV2Ack(caption.utteranceId, caption.version),
          ),
        };
      }
      if (label === "captions") {
        const result = decodeCaptionV1(bytes);
        const outcome = noteResult(label, result, bytes, data);
        if (result.ok) emitCaption(label, result);
        return { outcome, captions: result.ok && result.text ? 1 : 0 };
      }
      if (label === "collections") {
        const result = decodeCollections(bytes);
        const outcome = noteResult(label, result, bytes, data);
        if (!result.ok) return { outcome, captions: 0 };
        if (result.participants.length) mergeParticipants(result.participants);
        if (result.chats.length) emitChats(result.chats);
        return { outcome, captions: 0 };
      }
      const result = decodeMeetMessage(bytes);
      const outcome = noteResult(label, result, bytes, data);
      if (result.ok) emitChats(result.chats);
      return { outcome, captions: 0 };
    }

    return {
      frame,
      // Base64 response of SyncMeetingSpaceCollections.
      syncCollectionsResponse(base64Text) {
        const result = decodeSyncCollections(base64ToBytes(base64Text));
        if (result.ok) mergeParticipants(result.participants);
        return result.ok;
      },
      // Base64 response of CreateMeetingMessage: the user's own sent chat.
      createMessageResponse(base64Text) {
        const result = decodeMeetMessage(base64ToBytes(base64Text));
        if (result.ok) emitChats(result.chats);
        return result.ok;
      },
      // A request or response that names the local device. Field 7 in the roster wins.
      noteSelfDevice(bytesOrText) {
        if (selfDeviceId) return;
        const deviceId = findDeviceName(bytesOrText);
        if (!deviceId) return;
        selfDeviceId = deviceId;
        emitRosterIfChanged();
      },
      // A new meeting connection is a new call, with its own budget of reported failures.
      newCall() {
        for (const counter of Object.values(counters)) counter.reported = 0;
      },
      lastRoster: () => lastRoster,
      hasRoster: () => lastRoster !== undefined,
      parseFailures,
      counters: () => JSON.parse(JSON.stringify(counters)),
    };
  }

  globalThis.mcMeetWire = Object.freeze({
    bytesToBase64,
    parseFields,
    inflateFrame,
    skeleton,
    encodeCaptionV2Ack,
    decodeCaptionV2,
    decodeCaptionV1,
    decodeCollections,
    decodeMeetMessage,
    decodeSyncCollections,
    createMeetReader,
  });
})();
