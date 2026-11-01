// Page hook. Runs in the Meet page (MAIN world) at document_start, after page/wire.js.
// It wraps RTCPeerConnection to read Meet's data channels, opens the caption channels itself,
// and reads two Meet RPC responses through a fetch wrapper. It sends what it reads to the
// content script as JSON strings in "mc-meet:page" events, and takes commands from
// "mc-meet:content" events: listen, snapshot for debug material, and debug_mode. Once a second
// it posts caption channel telemetry: counters, lifecycle events and, in debug mode, raw frames.
// It makes no network requests of its own.
(() => {
  if (window.__mcMeetHookInstalled) return;
  window.__mcMeetHookInstalled = true;

  const wire = globalThis.mcMeetWire;
  const NativePeerConnection = window.RTCPeerConnection;
  const CAPTION_LABELS = ["captions", "captions_v2"];
  const READ_LABELS = new Set([...CAPTION_LABELS, "collections", "meet_messages"]);
  const RPC_BASE = "https://meet.google.com/$rpc/google.rtc.meetings.v1.";
  const SYNC_COLLECTIONS_URL = `${RPC_BASE}MeetingSpaceService/SyncMeetingSpaceCollections`;
  const CREATE_MESSAGE_URL = `${RPC_BASE}MeetingMessageService/CreateMeetingMessage`;
  const CREATE_DEVICE_URL = `${RPC_BASE}MeetingDeviceService/CreateMeetingDevice`;
  const MAX_EARLY_MESSAGES = 2000;
  const REOPEN_ATTEMPTS = 5;
  const REOPEN_DELAY_MS = 2000;
  // Data channels a snapshot lists, newest last.
  const SNAPSHOT_CHANNELS = 30;
  const TELEMETRY_INTERVAL_MS = 1000;
  // Bounds on what one telemetry post carries, so a burst cannot grow the page's memory.
  const MAX_TELEMETRY_EVENTS = 200;
  const MAX_TELEMETRY_FRAMES = 1000;
  // RTCErrorEvent.error.errorDetail values in the WebRTC spec. Anything else reads "unknown".
  const CHANNEL_ERROR_DETAILS = new Set([
    "data-channel-failure",
    "dtls-failure",
    "fingerprint-failure",
    "sctp-failure",
    "sdp-syntax-error",
    "hardware-encoder-not-available",
    "hardware-encoder-error",
  ]);

  // Messages wait here until the content script says it listens.
  const earlyMessages = [];
  let isContentListening = false;
  let meetingConnection;
  let captionChannelsOpenedAt;
  const ownChannels = new WeakSet();
  const watchedChannels = new WeakSet();
  // { label, isOwn, channel } per channel seen, newest last. A closed channel's entry lets go of
  // it, so Meet's closed channels and their connections can be freed.
  const seenChannels = [];
  // Telemetry of each caption channel, by channel. A closed channel's entry goes after the next
  // post, which carries its final counts.
  const captionChannelStats = new Map();
  // Numbers the caption channels this hook tracks, from 1. A rejoin's channel can have the label
  // and id of one before it; the serial keeps their counters apart.
  let lastChannelSerial = 0;
  let telemetryEvents = [];
  let telemetryFrames = [];
  // Raw frames are copied only while the app's debug mode runs: base64 costs page time.
  let debugModeUntilMs = 0;

  function post(message) {
    if (!isContentListening) {
      if (earlyMessages.length < MAX_EARLY_MESSAGES) earlyMessages.push(message);
      return;
    }
    document.dispatchEvent(new CustomEvent("mc-meet:page", { detail: JSON.stringify(message) }));
  }

  function postHookState() {
    post({
      t: "hook_state",
      installed: Boolean(wire && NativePeerConnection),
      caption_channels_opened_at: captionChannelsOpenedAt ?? null,
    });
  }

  if (!wire || !NativePeerConnection) {
    document.addEventListener("mc-meet:content", () => {
      isContentListening = true;
      postHookState();
    });
    document.dispatchEvent(new CustomEvent("mc-meet:hook-ready"));
    return;
  }

  const reader = wire.createMeetReader({
    emit: (reading) => post({ t: "reading", reading }),
    notice: (notice) => post({ t: "notice", notice }),
  });

  async function frameBytes(data) {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data))
      return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof Blob) return new Uint8Array(await data.arrayBuffer());
    return undefined;
  }

  // ---- Caption channel telemetry. The frame path does integer increments only.

  function channelAttributes(channel) {
    const attributes = {
      "mc.caption.channel.label": channel.label,
      "mc.caption.channel.is_own": ownChannels.has(channel),
      "mc.caption.channel.negotiated": channel.negotiated === true,
    };
    if (channel.id !== null) attributes["mc.caption.channel.id"] = channel.id;
    const stats = captionChannelStats.get(channel);
    if (stats) attributes["mc.caption.channel.serial"] = stats.serial;
    return attributes;
  }

  function channelOptionAttributes(channel) {
    const attributes = {
      ...channelAttributes(channel),
      "mc.caption.channel.ordered": channel.ordered === true,
    };
    if (channel.maxRetransmits !== null) {
      attributes["mc.caption.channel.max_retransmits"] = channel.maxRetransmits;
    }
    return attributes;
  }

  function noteTelemetryEvent(name, attributes) {
    if (telemetryEvents.length >= MAX_TELEMETRY_EVENTS) return;
    telemetryEvents.push({ at_ms: Date.now(), name, attributes });
  }

  function trackCaptionChannel(channel) {
    lastChannelSerial += 1;
    const stats = {
      channel,
      serial: lastChannelSerial,
      started_at_ms: Date.now(),
      raw: 0,
      parsed: 0,
      control: 0,
      rejected: 0,
      acks: 0,
      isFirstFrameSeen: false,
      isFirstCaptionSeen: false,
      isClosed: false,
    };
    captionChannelStats.set(channel, stats);
    const noteOpened = () =>
      noteTelemetryEvent("mc.caption.channel.opened", channelOptionAttributes(channel));
    if (channel.readyState === "open") noteOpened();
    else channel.addEventListener("open", noteOpened, { once: true });
    channel.addEventListener("close", () => noteChannelClosed(stats));
    channel.addEventListener("error", (errorEvent) => {
      const detail = errorEvent?.error?.errorDetail;
      noteTelemetryEvent("mc.caption.channel.error", {
        ...channelAttributes(channel),
        "error.type": CHANNEL_ERROR_DETAILS.has(detail) ? detail : "unknown",
      });
    });
    return stats;
  }

  function noteChannelClosed(stats) {
    if (stats.isClosed) return;
    stats.isClosed = true;
    noteTelemetryEvent("mc.caption.channel.closed", {
      ...channelAttributes(stats.channel),
      "mc.caption.channel.lifetime_ms": Date.now() - stats.started_at_ms,
    });
  }

  function noteFrameOutcome(stats, outcome, captions) {
    if (outcome === "parsed") stats.parsed += 1;
    else if (outcome === "control") stats.control += 1;
    else if (outcome === "rejected") stats.rejected += 1;
    if (captions > 0 && !stats.isFirstCaptionSeen) {
      stats.isFirstCaptionSeen = true;
      noteTelemetryEvent("mc.caption.first_caption", {
        ...channelAttributes(stats.channel),
        "mc.caption.channel.age_ms": Date.now() - stats.started_at_ms,
      });
    }
  }

  function noteRawFrame(stats, bytes) {
    if (Date.now() >= debugModeUntilMs || telemetryFrames.length >= MAX_TELEMETRY_FRAMES) return;
    const channel = stats.channel;
    telemetryFrames.push({
      channel: channel.label,
      channel_id: channel.id,
      is_own: ownChannels.has(channel),
      at_ms: Date.now(),
      byte_length: bytes.length,
      bytes_base64: wire.bytesToBase64(bytes),
    });
  }

  function postTelemetry() {
    // Early messages wait for the content script; telemetry posts would push them out.
    if (!isContentListening) return;
    const isIdle =
      meetingConnection === undefined &&
      captionChannelStats.size === 0 &&
      telemetryEvents.length === 0;
    if (isIdle) return;
    const channels = [];
    for (const stats of captionChannelStats.values()) {
      const channel = stats.channel;
      // RTCPeerConnection.close() closes its channels and fires no close event on them.
      if (channel.readyState === "closed") noteChannelClosed(stats);
      // A channel has an id once its transport is up, and no frame comes before that.
      if (channel.id !== null) {
        channels.push({
          label: channel.label,
          id: channel.id,
          serial: stats.serial,
          is_own: ownChannels.has(channel),
          started_at_ms: stats.started_at_ms,
          raw: stats.raw,
          parsed: stats.parsed,
          control: stats.control,
          rejected: stats.rejected,
          acks: stats.acks,
        });
      }
      if (stats.isClosed) captionChannelStats.delete(channel);
    }
    post({
      t: "telemetry",
      at_ms: Date.now(),
      channels,
      events: telemetryEvents,
      frames: telemetryFrames,
    });
    telemetryEvents = [];
    telemetryFrames = [];
  }

  setInterval(postTelemetry, TELEMETRY_INTERVAL_MS);

  function watchChannel(channel) {
    if (watchedChannels.has(channel) || !READ_LABELS.has(channel.label)) return;
    watchedChannels.add(channel);
    const label = channel.label;
    const stats = CAPTION_LABELS.includes(label) ? trackCaptionChannel(channel) : undefined;
    // Gzip inflation is async. The chain keeps frames of one channel in arrival order.
    let chain = Promise.resolve();
    channel.addEventListener("message", (messageEvent) => {
      const data = messageEvent.data;
      if (stats) {
        stats.raw += 1;
        if (!stats.isFirstFrameSeen) {
          stats.isFirstFrameSeen = true;
          noteTelemetryEvent("mc.caption.first_frame", {
            ...channelAttributes(channel),
            "mc.caption.channel.age_ms": Date.now() - stats.started_at_ms,
          });
        }
      }
      chain = chain
        .then(async () => {
          const bytes = await frameBytes(data);
          // A text frame on a caption channel is not Meet's caption protocol.
          if (!bytes) {
            if (stats) stats.rejected += 1;
            return;
          }
          if (stats) noteRawFrame(stats, bytes);
          let reading;
          try {
            reading = await reader.frame(label, bytes);
          } catch {
            if (stats) stats.rejected += 1;
            return;
          }
          const { acks = [], outcome, captions = 0 } = reading;
          if (stats) noteFrameOutcome(stats, outcome, captions);
          if (!ownChannels.has(channel)) return;
          for (const ack of acks) {
            if (channel.readyState !== "open") continue;
            channel.send(ack);
            if (stats) stats.acks += 1;
          }
        })
        .catch(() => {});
    });
  }

  function openCaptionChannel(connection, label, attempt) {
    let channel;
    try {
      channel = connection.createDataChannel(label, { ordered: true, maxRetransmits: 10 });
    } catch (error) {
      post({ t: "caption_channel_failed", label, error: String(error?.message ?? error) });
      return;
    }
    channel.binaryType = "arraybuffer";
    ownChannels.add(channel);
    noteChannel(channel);
    watchChannel(channel);
    channel.addEventListener("close", () => {
      const isLive =
        connection.connectionState !== "closed" && connection.connectionState !== "failed";
      if (isLive && meetingConnection === connection && attempt < REOPEN_ATTEMPTS) {
        setTimeout(() => openCaptionChannel(connection, label, attempt + 1), REOPEN_DELAY_MS);
      }
    });
  }

  // The connection that carries "collections" is the meeting's. Open caption channels on it once.
  // Meet then streams captions to us without the captions button. The language stays Meet's.
  function adoptMeetingConnection(connection) {
    if (meetingConnection === connection) return;
    meetingConnection = connection;
    reader.newCall();
    for (const label of CAPTION_LABELS) openCaptionChannel(connection, label, 0);
    captionChannelsOpenedAt = Date.now();
    postHookState();
  }

  function noteChannel(channel) {
    const entry = { label: channel.label, isOwn: ownChannels.has(channel), channel };
    channel.addEventListener("close", () => {
      entry.channel = null;
    });
    seenChannels.push(entry);
    if (seenChannels.length > SNAPSHOT_CHANNELS) seenChannels.shift();
  }

  function onChannel(connection, channel) {
    noteChannel(channel);
    watchChannel(channel);
    if (channel.label === "collections") adoptMeetingConnection(connection);
  }

  const nativeCreateDataChannel = NativePeerConnection.prototype.createDataChannel;
  NativePeerConnection.prototype.createDataChannel = function (...args) {
    const channel = nativeCreateDataChannel.apply(this, args);
    try {
      if (channel && !ownChannels.has(channel)) onChannel(this, channel);
    } catch {}
    return channel;
  };

  // Turning Meet's captions off closes Meet's caption channel, and Meet then stops streaming
  // captions to this browser. Keeping that channel open keeps captions coming with CC off.
  const nativeCloseChannel = RTCDataChannel.prototype.close;
  RTCDataChannel.prototype.close = function (...args) {
    if (CAPTION_LABELS.includes(this.label) && !ownChannels.has(this)) {
      try {
        noteTelemetryEvent("mc.caption.channel.close_blocked", channelOptionAttributes(this));
      } catch {}
      return;
    }
    return nativeCloseChannel.apply(this, args);
  };

  const WrappedPeerConnection = new Proxy(NativePeerConnection, {
    construct(target, args, newTarget) {
      const connection = Reflect.construct(target, args, newTarget);
      try {
        connection.addEventListener("datachannel", (event) => onChannel(connection, event.channel));
        connection.addEventListener("connectionstatechange", () => {
          const state = connection.connectionState;
          if ((state === "closed" || state === "failed") && meetingConnection === connection) {
            meetingConnection = undefined;
            // A rejoin in this tab must wait for the new connection's channels.
            captionChannelsOpenedAt = undefined;
            postHookState();
          }
        });
      } catch {}
      return connection;
    },
  });
  window.RTCPeerConnection = WrappedPeerConnection;
  if (window.webkitRTCPeerConnection === NativePeerConnection) {
    window.webkitRTCPeerConnection = WrappedPeerConnection;
  }

  function requestUrl(input) {
    if (typeof input === "string") return input;
    if (input instanceof URL) return input.href;
    if (input instanceof Request) return input.url;
    return "";
  }

  function readResponseText(promise, handle) {
    promise
      .then((response) => response.clone().text())
      .then(handle)
      .catch(() => {});
  }

  const nativeFetch = window.fetch;
  window.fetch = function (...args) {
    const promise = nativeFetch.apply(this, args);
    try {
      const url = requestUrl(args[0]);
      if (url === SYNC_COLLECTIONS_URL) {
        const body = args[1]?.body;
        if (typeof body === "string" || body instanceof Uint8Array || body instanceof ArrayBuffer) {
          reader.noteSelfDevice(typeof body === "string" ? body : new Uint8Array(body));
        }
        readResponseText(promise, (text) => reader.syncCollectionsResponse(text));
      } else if (url === CREATE_MESSAGE_URL) {
        readResponseText(promise, (text) => reader.createMessageResponse(text));
      } else if (url === CREATE_DEVICE_URL) {
        readResponseText(promise, (text) => {
          const binary = atob(text.trim());
          reader.noteSelfDevice(binary);
        });
      }
    } catch {}
    return promise;
  };

  document.addEventListener("mc-meet:content", (event) => {
    let command;
    try {
      command = JSON.parse(event.detail);
    } catch {
      return;
    }
    if (command?.t === "snapshot") {
      postSnapshot();
      return;
    }
    if (command?.t === "debug_mode") {
      debugModeUntilMs = Number.isFinite(command.until_ms) ? command.until_ms : 0;
      return;
    }
    if (command?.t !== "listen") return;
    // A content script that starts after the hook (a reload of the extension) gets the
    // early messages once and the current roster again.
    const isFirstListener = !isContentListening;
    isContentListening = true;
    postHookState();
    if (isFirstListener) for (const message of earlyMessages.splice(0)) post(message);
    const roster = reader.lastRoster();
    if (roster) post({ t: "reading", reading: roster });
  });

  // Channel states, frame counters and Meet's build. No frame content.
  function postSnapshot() {
    post({
      t: "page_snapshot",
      page: {
        channels: seenChannels.map((entry) => ({
          label: entry.label,
          ready_state: entry.channel?.readyState ?? "closed",
          is_own: entry.isOwn,
        })),
        has_meeting_connection: meetingConnection !== undefined,
        meeting_connection_state: meetingConnection?.connectionState ?? null,
        caption_channels_opened_at: captionChannelsOpenedAt ?? null,
        frame_counters: reader.counters(),
        // Meet's front-end build label, when the page exposes it.
        meet_build: window.WIZ_global_data?.cfb2h ?? null,
      },
    });
  }

  // The content script may have loaded first and missed nothing but this signal.
  document.dispatchEvent(new CustomEvent("mc-meet:hook-ready"));
})();
