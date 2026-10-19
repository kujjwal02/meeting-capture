// Content script, isolated world. It owns the call session of its tab: it finds call start and
// end, forwards what the two caption readers emit, starts and stops the DOM reader as the caption
// source policy decides, and reports health. Events go to the service worker over a port as they
// happen.
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
  const ROSTER_WAIT_MS = 30000;
  const DOM_ROSTER_EVERY_MS = 10000;
  const KEEPALIVE_MS = 20000;
  const MAX_OUTBOX = 5000;
  const MEETING_CODE_PATTERN = /^\/([a-z]{3}-[a-z]{4}-[a-z]{3})(?:\/|$)/;

  let call = null;
  let leaveMissingSinceMs = null;
  let lastHealthKey = "";
  let isSelfRemoved = false;
  let selfName = "";
  let isRosterFromChannel = false;
  let lastRosterKey = "";
  let channelRoster = null;
  let lastDomRosterMs = 0;
  let lastPingMs = 0;
  let port = null;
  let isContextValid = true;
  const outbox = [];

  const captionSource = globalThis.mcMeetCreateCaptionSourcePolicy();
  const domCaptions = globalThis.mcMeetCreateDomCaptions({
    emit: (event) => onReaderEvent("dom", event),
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
      follow(captionSource.decide({ kind: "session_dropped" }));
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
    lastRosterKey = "";
    lastHealthKey = "";
    if (channelRoster) sendRoster(channelRoster);
    follow(captionSource.decide({ kind: "call_began", atMs: call.page_started_ms }));
  }

  function endCall(reason) {
    send(callEvents.callEnded({ atMs: Date.now(), reason }));
    try {
      sessionStorage.removeItem(SESSION_KEY);
    } catch {}
    call = null;
    follow(captionSource.decide({ kind: "call_ended" }));
    isSelfRemoved = false;
  }

  // ---- Caption source and health.

  function areCaptionsOn() {
    return Boolean(findIconButton(selectors.captionsOnIcon));
  }

  function reportHealth() {
    if (!call) return;
    const { source, parseFailures, detail } = captionSource.health();
    const health = { source, captionsOn: areCaptionsOn(), parseFailures, detail };
    const key = JSON.stringify(health);
    if (key === lastHealthKey) return;
    lastHealthKey = key;
    send(callEvents.health({ atMs: Date.now(), ...health }));
  }

  // Runs the DOM reader exactly while the policy picks it. start and stop are idempotent.
  function follow(decision) {
    if (decision.source === "dom") domCaptions.start();
    else domCaptions.stop();
    if (decision.reportsHealth) reportHealth();
  }

  // Meet sends nothing on the caption channels until its captions are on, and the DOM reader
  // needs them visible. Never touches the caption language.
  function clickCaptionsOn(nowMs) {
    const button = findIconButton(selectors.captionsOffIcon);
    if (!button) return;
    captionSource.decide({ kind: "captions_clicked", atMs: nowMs });
    button.click();
  }

  // ---- Reader events. Both caption readers deliver spec events here; origin names the reader.

  function onReaderEvent(origin, event) {
    if (event.kind === "roster") {
      isRosterFromChannel = true;
      selfName = event.participants.find((p) => p.is_self)?.name ?? selfName;
      channelRoster = event.participants;
      sendRoster(event.participants);
      return;
    }
    if (!call) return;
    if (event.kind === "caption") {
      if (origin === "channel") {
        follow(captionSource.decide({ kind: "channel_caption", atMs: Date.now() }));
      }
      if (captionSource.health().source !== origin) return;
    }
    send(event);
  }

  function onPageNotice(notice) {
    if (notice.kind === "self_removed") {
      isSelfRemoved = true;
    } else if (notice.kind === "parse_gap") {
      follow(
        captionSource.decide({
          kind: "parse_gap",
          atMs: Date.now(),
          channel: notice.channel,
          parseFailures: notice.parse_failures,
          detail: notice.detail,
          isInCall: Boolean(call),
        }),
      );
    }
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
      const channelsOpenedAtMs = message.caption_channels_opened_at;
      follow(captionSource.decide({ kind: "hook_state", channelsOpenedAtMs }));
    } else if (message?.t === "event") {
      onReaderEvent("channel", message.event);
    } else if (message?.t === "notice") {
      onPageNotice(message.notice);
    } else if (message?.t === "caption_channel_failed") {
      const { label, error } = message;
      follow(captionSource.decide({ kind: "channel_open_failed", label, error }));
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

    const decision = captionSource.decide({ kind: "tick", atMs: nowMs });
    follow(decision);
    if (decision.isCaptionsClickDue) clickCaptionsOn(nowMs);

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
