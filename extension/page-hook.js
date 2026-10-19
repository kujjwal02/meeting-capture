// Page hook. Runs in the Meet page (MAIN world) at document_start, after page/wire.js.
// It wraps RTCPeerConnection to read Meet's data channels, opens the caption channels itself,
// and reads two Meet RPC responses through a fetch wrapper. It sends what it reads to the
// content script as JSON strings in "mc-meet:page" events, and takes commands from
// "mc-meet:content" events. It makes no network requests of its own.
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

  // Messages wait here until the content script says it listens.
  const earlyMessages = [];
  let isContentListening = false;
  let meetingConnection;
  let captionChannelsOpenedAt;
  const ownChannels = new WeakSet();
  const watchedChannels = new WeakSet();

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

  const reader = wire.createMeetReader({ emit: (event) => post({ t: "reader", event }) });

  async function frameBytes(data) {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data))
      return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof Blob) return new Uint8Array(await data.arrayBuffer());
    return undefined;
  }

  function watchChannel(channel) {
    if (watchedChannels.has(channel) || !READ_LABELS.has(channel.label)) return;
    watchedChannels.add(channel);
    const label = channel.label;
    // Gzip inflation is async. The chain keeps frames of one channel in arrival order.
    let chain = Promise.resolve();
    channel.addEventListener("message", (messageEvent) => {
      const data = messageEvent.data;
      chain = chain
        .then(async () => {
          const bytes = await frameBytes(data);
          if (!bytes) return;
          const { ack } = await reader.frame(label, bytes);
          if (ack && ownChannels.has(channel) && channel.readyState === "open") channel.send(ack);
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
    for (const label of CAPTION_LABELS) openCaptionChannel(connection, label, 0);
    captionChannelsOpenedAt = Date.now();
    postHookState();
  }

  function onChannel(connection, channel) {
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

  const WrappedPeerConnection = new Proxy(NativePeerConnection, {
    construct(target, args, newTarget) {
      const connection = Reflect.construct(target, args, newTarget);
      try {
        connection.addEventListener("datachannel", (event) => onChannel(connection, event.channel));
        connection.addEventListener("connectionstatechange", () => {
          const state = connection.connectionState;
          if ((state === "closed" || state === "failed") && meetingConnection === connection) {
            meetingConnection = undefined;
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
    if (command?.t !== "listen") return;
    // A content script that starts after the hook (a reload of the extension) gets the
    // early messages once and the current roster again.
    const isFirstListener = !isContentListening;
    isContentListening = true;
    postHookState();
    if (isFirstListener) for (const message of earlyMessages.splice(0)) post(message);
    const roster = reader.lastRoster();
    if (roster) post({ t: "reader", event: roster });
  });

  // The content script may have loaded first and missed nothing but this signal.
  document.dispatchEvent(new CustomEvent("mc-meet:hook-ready"));
})();
