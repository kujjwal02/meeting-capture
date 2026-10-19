// Content script, isolated world. It owns the call session of its tab: it finds call start and
// end, forwards what the page hook reads, falls back to the DOM when the channels give nothing,
// and reports health. Events go to the service worker over a port as they happen.
(() => {
  if (globalThis.mcMeetContentStarted) return;
  globalThis.mcMeetContentStarted = true;

  const selectors = globalThis.mcMeetSelectors;
  const findIconButton = globalThis.mcMeetFindIconButton;
  const callEvents = globalThis.mcMeetCallEvents;

  const SESSION_KEY = "mc-meet-session";
  // A reload of the same meeting in this tab within this window keeps the session.
  const RESUME_WINDOW_MS = 5 * 60 * 1000;
  const LEAVE_GRACE_MS = 3000;
  const CHECK_MS = 1000;
  // The DOM reader starts when no caption arrives this long after the caption channels open.
  const CAPTION_WAIT_MS = 20000;
  const ROSTER_WAIT_MS = 30000;
  const DOM_ROSTER_EVERY_MS = 10000;
  const KEEPALIVE_MS = 20000;
  const CAPTIONS_CLICK_ATTEMPTS = 3;
  const CAPTIONS_CLICK_GAP_MS = 2000;
  const MAX_OUTBOX = 5000;
  const MEETING_CODE_PATTERN = /^\/([a-z]{3}-[a-z]{4}-[a-z]{3})(?:\/|$)/;

  let call = null;
  let leaveMissingSinceMs = null;
  let hookState = { installed: false, caption_channels_opened_at: null };
  let source = "none";
  let parseFailures = 0;
  let healthDetail = null;
  let lastHealthKey = "";
  let lastChannelCaptionMs = null;
  let isSelfRemoved = false;
  let selfName = "";
  let isRosterFromChannel = false;
  let lastRosterKey = "";
  let channelRoster = null;
  let lastDomRosterMs = 0;
  let captionsClicks = 0;
  let lastCaptionsClickMs = 0;
  let lastPingMs = 0;
  let port = null;
  let isContextValid = true;
  const outbox = [];

  const domCaptions = globalThis.mcMeetCreateDomCaptions({
    emit: (event) => {
      if (call && source === "dom") send(event);
    },
    selfName: () => selfName,
  });

  // ---- Worker port.

  function onWorkerMessage(message) {
    // The worker ended this session while the page was away. Start a new one on the next check.
    if (message?.type === "session_ended" && call?.session_id === message.session_id) {
      try {
        sessionStorage.removeItem(SESSION_KEY);
      } catch {}
      call = null;
      domCaptions.stop();
      source = "none";
      return;
    }
    if (message?.type !== "stored") return;
    for (let i = outbox.length - 1; i >= 0; i--) {
      const queued = outbox[i];
      if (queued.session_id === message.session_id && queued.n <= message.n) outbox.splice(i, 1);
    }
  }

  function connectPort() {
    if (port || !isContextValid) return;
    try {
      port = chrome.runtime.connect({ name: "mc-meet-tab" });
    } catch {
      shutDown();
      return;
    }
    port.onMessage.addListener(onWorkerMessage);
    port.onDisconnect.addListener(() => {
      void chrome.runtime.lastError;
      port = null;
    });
    if (call) postToWorker({ type: "resume", session_id: call.session_id });
    for (const message of outbox) postToWorker(message);
  }

  function postToWorker(message) {
    if (!port) return;
    try {
      port.postMessage(message);
    } catch {
      port = null;
    }
  }

  // The extension was reloaded or removed. This copy of the script can no longer reach it.
  function shutDown() {
    isContextValid = false;
    clearInterval(checkTimer);
    domCaptions.stop();
  }

  // ---- Session.

  function readStoredSession() {
    try {
      return JSON.parse(sessionStorage.getItem(SESSION_KEY) ?? "null");
    } catch {
      return null;
    }
  }

  function storeSession() {
    if (!call) return;
    call.last_seen_ms = Date.now();
    try {
      sessionStorage.setItem(SESSION_KEY, JSON.stringify(call));
    } catch {}
  }

  function send(event) {
    if (!call) return;
    const message = { type: "event", session_id: call.session_id, n: call.next_n, event };
    call.next_n += 1;
    storeSession();
    outbox.push(message);
    if (outbox.length > MAX_OUTBOX) outbox.shift();
    postToWorker(message);
  }

  function meetingCode() {
    const match = location.pathname.match(MEETING_CODE_PATTERN);
    if (match) return match[1];
    return location.pathname.replace(/^\/+|\/+$/g, "") || "unknown";
  }

  function meetingTitle(code) {
    const titleElement = document.querySelector(selectors.meetingTitle);
    const fromAttribute = titleElement?.getAttribute("data-meeting-title")?.trim();
    if (fromAttribute) return fromAttribute;
    const fromDocument = document.title.replace(/^(Google )?Meet\s*[-–:]\s*/, "").trim();
    if (!fromDocument || fromDocument === code || /^(Google )?Meet$/.test(fromDocument)) return "";
    return fromDocument;
  }

  function beginCall() {
    const code = meetingCode();
    const stored = readStoredSession();
    leaveMissingSinceMs = null;
    if (stored?.meeting_code === code && Date.now() - stored.last_seen_ms < RESUME_WINDOW_MS) {
      call = stored;
      storeSession();
      postToWorker({ type: "resume", session_id: call.session_id });
    } else {
      const startedMs = Date.now();
      call = {
        session_id: crypto.randomUUID(),
        meeting_code: code,
        started_at: new Date(startedMs).toISOString(),
        started_ms: startedMs,
        next_n: 1,
        last_seen_ms: Date.now(),
      };
      send(
        callEvents.callStarted({ atMs: startedMs, meetingCode: code, title: meetingTitle(code) }),
      );
    }
    call.page_started_ms = Date.now();
    source = "none";
    lastChannelCaptionMs = null;
    lastRosterKey = "";
    lastHealthKey = "";
    if (channelRoster) sendRoster(channelRoster);
    reportHealth();
  }

  function endCall(reason) {
    send(callEvents.callEnded({ atMs: Date.now(), reason }));
    try {
      sessionStorage.removeItem(SESSION_KEY);
    } catch {}
    call = null;
    domCaptions.stop();
    source = "none";
    captionsClicks = 0;
    isSelfRemoved = false;
  }

  // ---- Caption source and health.

  function areCaptionsOn() {
    return Boolean(findIconButton(selectors.captionsOnIcon));
  }

  function reportHealth() {
    if (!call) return;
    const health = {
      source,
      captionsOn: areCaptionsOn(),
      parseFailures,
      detail: healthDetail,
    };
    const key = JSON.stringify(health);
    if (key === lastHealthKey) return;
    lastHealthKey = key;
    send(callEvents.health({ atMs: Date.now(), ...health }));
  }

  function useDom(reason) {
    if (source === "dom") return;
    source = "dom";
    healthDetail = reason;
    captionsClicks = 0;
    domCaptions.start();
    reportHealth();
  }

  function useChannel() {
    if (source === "channel") return;
    source = "channel";
    healthDetail = null;
    domCaptions.stop();
    reportHealth();
  }

  // Meet sends nothing on the caption channels until its captions are on, and the DOM reader
  // needs them visible. Never touches the caption language.
  function clickCaptionsOnIfOff(nowMs) {
    if (
      captionsClicks >= CAPTIONS_CLICK_ATTEMPTS ||
      nowMs - lastCaptionsClickMs < CAPTIONS_CLICK_GAP_MS
    ) {
      return;
    }
    const button = findIconButton(selectors.captionsOffIcon);
    if (!button) return;
    captionsClicks += 1;
    lastCaptionsClickMs = nowMs;
    button.click();
  }

  function captionWaitStartMs() {
    return Math.max(call.page_started_ms, hookState.caption_channels_opened_at ?? 0);
  }

  // ---- Page hook messages.

  function onReaderEvent(event) {
    if (event.kind === "self_removed") {
      isSelfRemoved = true;
      return;
    }
    if (event.kind === "parse_gap") {
      parseFailures = event.parse_failures;
      healthDetail = event.detail;
      const isCaptionChannel = event.channel === "captions" || event.channel === "captions_v2";
      const isChannelQuiet =
        lastChannelCaptionMs === null || Date.now() - lastChannelCaptionMs > CAPTION_WAIT_MS;
      if (call && isCaptionChannel && isChannelQuiet) useDom(event.detail);
      else reportHealth();
      return;
    }
    if (event.kind === "roster") {
      isRosterFromChannel = true;
      selfName = event.participants.find((p) => p.is_self)?.name ?? selfName;
      channelRoster = event.participants;
      sendRoster(event.participants);
      return;
    }
    if (!call) return;
    if (event.kind === "caption") {
      lastChannelCaptionMs = Date.now();
      useChannel();
      send(event);
      return;
    }
    if (event.kind === "chat") send(event);
  }

  function sendRoster(participants) {
    if (!call || participants.length === 0) return;
    const key = JSON.stringify(participants);
    if (key === lastRosterKey) return;
    lastRosterKey = key;
    send(callEvents.roster({ atMs: Date.now(), participants }));
  }

  function onPageMessage(message) {
    if (message?.t === "hook_state") {
      hookState = message;
      reportHealth();
    } else if (message?.t === "reader") {
      onReaderEvent(message.event);
    } else if (message?.t === "caption_channel_failed") {
      healthDetail = `opening ${message.label} failed: ${message.error}`;
      reportHealth();
    }
  }

  document.addEventListener("mc-meet:page", (event) => {
    if (!isContextValid) return;
    let message;
    try {
      message = JSON.parse(event.detail);
    } catch {
      return;
    }
    onPageMessage(message);
  });

  function askHookToTalk() {
    document.dispatchEvent(
      new CustomEvent("mc-meet:content", { detail: JSON.stringify({ t: "listen" }) }),
    );
  }
  document.addEventListener("mc-meet:hook-ready", askHookToTalk);
  askHookToTalk();

  // ---- The check loop. A live call keeps WebRTC busy, so Chrome does not throttle this timer
  // below once a second. Events never wait for it: they are sent as they arrive.

  function check() {
    if (!chrome.runtime?.id) {
      shutDown();
      return;
    }
    const nowMs = Date.now();
    connectPort();
    if (port && nowMs - lastPingMs >= KEEPALIVE_MS) {
      lastPingMs = nowMs;
      postToWorker({ type: "ping" });
    }

    const isInCall = Boolean(findIconButton(selectors.leaveCallIcon));
    if (isInCall) {
      leaveMissingSinceMs = null;
      if (!call) beginCall();
    } else if (call) {
      leaveMissingSinceMs ??= nowMs;
      if (nowMs - leaveMissingSinceMs >= LEAVE_GRACE_MS) endCall(isSelfRemoved ? "kicked" : "left");
    }
    if (!call) return;
    if (nowMs - call.last_seen_ms >= 5000) storeSession();

    if (source === "none" && nowMs - captionWaitStartMs() >= CAPTION_WAIT_MS) {
      useDom(
        hookState.caption_channels_opened_at
          ? "no caption frame within 20 s of opening the caption channels"
          : "page hook did not open the caption channels",
      );
    }
    clickCaptionsOnIfOff(nowMs);

    const isRosterLate = nowMs - call.page_started_ms >= ROSTER_WAIT_MS;
    if (!isRosterFromChannel && isRosterLate && nowMs - lastDomRosterMs >= DOM_ROSTER_EVERY_MS) {
      lastDomRosterMs = nowMs;
      sendRoster(globalThis.mcMeetReadDomRoster());
    }
    reportHealth();
  }

  const checkTimer = setInterval(check, CHECK_MS);
  window.addEventListener("pagehide", storeSession);
  connectPort();
})();
