// Content script, isolated world. It owns the call session of its tab: it finds call start and
// end, forwards what the two caption readers emit, starts and stops the DOM reader as the caption
// source policy decides, and reports health. While the app reports the call as stopped by the
// user, it sends no caption or chat. Events go to the service worker over a port as they
// happen. Debug material (failed frames, page snapshots) goes the same way but is never queued.
(() => {
  if (globalThis.mcMeetContentStarted) return;
  globalThis.mcMeetContentStarted = true;

  const selectors = globalThis.mcMeetSelectors;
  const findIconButton = globalThis.mcMeetFindIconButton;
  const callEvents = globalThis.mcMeetCallEvents;

  const SESSION_KEY = "mc-meet-session";
  const SETTINGS_KEY = "settings";
  // A reload of the same meeting in this tab within this window keeps the session.
  const RESUME_WINDOW_MS = 5 * 60 * 1000;
  const LEAVE_GRACE_MS = 3000;
  const CHECK_MS = 1000;
  const ROSTER_WAIT_MS = 30000;
  const DOM_ROSTER_EVERY_MS = 10000;
  const KEEPALIVE_MS = 20000;
  const MAX_OUTBOX = 5000;
  // Meet serves calls at /<code> and at /meet/<code>.
  const MEETING_CODE_PATTERN = /^(?:\/meet)?\/([a-z]{3}-[a-z]{4}-[a-z]{3})(?:\/|$)/;
  // Failed frames sent to the app per session. The app keeps the first 20 per call.
  const FAILED_FRAMES_SENT = 20;
  // Only caption frame failures help debug a call that does not record. A chat or roster frame
  // with nothing to read is common, and its bytes hold chat.
  const CAPTION_CHANNELS = new Set(["captions", "captions_v2"]);
  // The page hook answers a snapshot request at once; a hook from an older mc-meet never does.
  const HOOK_SNAPSHOT_WAIT_MS = 2000;

  let call = null;
  // Kept out of the stored session: the worker tells a reloaded tab again.
  let isRecordingStopped = false;
  // Caption lines seen since the last stop or resume, and the lines a stop or resume cut. A cut
  // line that grows on would mix words from both sides of the change, so it is dropped.
  let liveUtterances = new Set();
  const cutUtterances = new Set();
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
  // The app's [meet] hide_captions. The worker stores it; on until the app says otherwise.
  let isHidingCaptions = true;
  const outbox = [];
  const failedFramesSent = new Map();
  // Snapshot requests waiting for the page hook's half, by session.
  const pendingSnapshots = new Map();

  const captionSource = globalThis.mcMeetCreateCaptionSourcePolicy();
  const domCaptions = globalThis.mcMeetCreateDomCaptions({
    emit: (event) => onReaderEvent("dom", event),
    selfName: () => selfName,
  });

  // ---- Settings. The worker stores what the app sends, so they hold while the app is away.

  function takeSettings(settings) {
    if (typeof settings?.hide_captions !== "boolean") return;
    isHidingCaptions = settings.hide_captions;
    domCaptions.setRegionHidden(isHidingCaptions);
  }

  chrome.storage.local
    .get(SETTINGS_KEY)
    .then((stored) => takeSettings(stored[SETTINGS_KEY]))
    .catch(() => {});
  chrome.storage.onChanged.addListener((changes, area) => {
    if (area === "local" && changes[SETTINGS_KEY]) takeSettings(changes[SETTINGS_KEY].newValue);
  });

  // ---- Worker port.

  function onWorkerMessage(message) {
    if (message?.type === "recording_state") {
      if (call?.session_id === message.session_id) setRecordingStopped(message.is_stopped === true);
      return;
    }
    if (message?.type === "snapshot_request") {
      if (call?.session_id === message.session_id) requestSnapshot(call.session_id);
      return;
    }
    // A restarted app knows nothing of the call's health; the next check resends it.
    if (message?.type === "app_connected") {
      lastHealthKey = "";
      return;
    }
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
    // A stopped call stays stopped in the app, which drops its captions until the worker says.
    isRecordingStopped = false;
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
    // The hook may have opened its channels before the page showed the call controls.
    askHookToTalk();
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

  // ---- Stop and resume. The app decides and logs both. The tab only stops sending captions and
  // chat, which the app would drop anyway.

  function setRecordingStopped(isStopped) {
    if (isStopped === isRecordingStopped) return;
    isRecordingStopped = isStopped;
    for (const key of liveUtterances) cutUtterances.add(key);
    liveUtterances = new Set();
  }

  // ---- Caption source and health.

  function reportHealth() {
    if (!call) return;
    const { source, parseFailures, detail, isCaptionFramesFailing } = captionSource.health();
    const button = globalThis.mcMeetReadCaptionsButton();
    const health = {
      source,
      captionsOn: button.isOn,
      captionBoxShown: globalThis.mcMeetIsCaptionBoxShown(),
      captionsUnavailable: button.isUnavailable,
      captionFramesFailing: isCaptionFramesFailing,
      parseFailures,
      detail,
    };
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

  // Meet streams nothing on the caption channels until something asks for captions. Pressing
  // Meet's captions control asks. Meet's captions button reads on about 30 s later, but Meet
  // renders no caption box, since the captions go to the hook's channels. A second press stops
  // the stream, so the policy asks for one press per call. Never touches the caption language.
  function requestCaptions(nowMs) {
    // Meet may ignore the press while the page has no focus, as a notification can take it.
    console.info("mc-meet: captions press", { has_focus: document.hasFocus() });
    const button = findIconButton(selectors.captionsOffIcon);
    if (button) {
      captionSource.decide({ kind: "captions_requested", atMs: nowMs });
      button.click();
      return;
    }
    // Behind the "Captions and translation" button the toggle sits in a popup. Meet's C
    // shortcut presses it without the popup.
    const menuButton = findIconButton(selectors.captionsMenuIcon);
    if (menuButton?.getAttribute("aria-pressed") !== "false") return;
    captionSource.decide({ kind: "captions_requested", atMs: nowMs });
    for (const type of ["keydown", "keyup"]) {
      document.body.dispatchEvent(
        new KeyboardEvent(type, {
          key: "c",
          code: "KeyC",
          keyCode: 67,
          bubbles: true,
          cancelable: true,
        }),
      );
    }
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
      const key = `${event.device_id}/${event.utterance_id}`;
      if (cutUtterances.has(key)) return;
      liveUtterances.add(key);
    }
    // The policy hears of channel captions during a stop too, or it would start the DOM reader.
    if (event.kind === "caption" && origin === "channel") {
      follow(captionSource.decide({ kind: "channel_caption", atMs: Date.now() }));
    }
    if (isRecordingStopped && (event.kind === "caption" || event.kind === "chat")) return;
    if (event.kind === "caption" && captionSource.health().source !== origin) return;
    send(event);
  }

  function forwardFailedFrame(notice) {
    if (!call || !CAPTION_CHANNELS.has(notice.channel)) return;
    const sent = failedFramesSent.get(call.session_id) ?? 0;
    if (sent >= FAILED_FRAMES_SENT) return;
    failedFramesSent.set(call.session_id, sent + 1);
    postToWorker({
      type: "frame_failed",
      session_id: call.session_id,
      channel: notice.channel,
      at: new Date(notice.at_ms).toISOString(),
      reason: notice.reason,
      skeleton: notice.skeleton,
      byte_length: notice.byte_length,
      bytes_base64: notice.bytes_base64,
    });
  }

  // ---- Page snapshot: what the page looked like when the app saw a problem. No caption text.

  function requestSnapshot(sessionId) {
    if (pendingSnapshots.has(sessionId)) return;
    const timer = setTimeout(() => finishSnapshot(sessionId, null), HOOK_SNAPSHOT_WAIT_MS);
    pendingSnapshots.set(sessionId, timer);
    document.dispatchEvent(
      new CustomEvent("mc-meet:content", { detail: JSON.stringify({ t: "snapshot" }) }),
    );
  }

  function finishSnapshot(sessionId, page) {
    const timer = pendingSnapshots.get(sessionId);
    if (timer === undefined) return;
    clearTimeout(timer);
    pendingSnapshots.delete(sessionId);
    const button = globalThis.mcMeetReadCaptionsButton();
    const chromeVersion = navigator.userAgent.match(/Chrome\/([\d.]+)/)?.[1] ?? null;
    postToWorker({
      type: "page_snapshot",
      session_id: sessionId,
      snapshot: {
        at: new Date().toISOString(),
        captions_button: {
          is_found: button.isFound,
          is_on: button.isOn,
          is_unavailable: button.isUnavailable,
          tooltip: button.tooltip,
        },
        caption_source: captionSource.health(),
        is_dom_reader_attached: domCaptions.isAttached(),
        caption_region: globalThis.mcMeetDescribeCaptionRegion(),
        page_hook: page,
        chrome_version: chromeVersion,
        extension_version: chrome.runtime.getManifest().version,
      },
    });
  }

  function onPageNotice(notice) {
    if (notice.kind === "frame_failed") {
      forwardFailedFrame(notice);
    } else if (notice.kind === "self_removed") {
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

  // The page hook reads frames in the MAIN world, which has no call-events.js.
  function eventFromReading(reading) {
    switch (reading?.kind) {
      case "caption":
        return callEvents.caption(reading);
      case "chat":
        return callEvents.chat(reading);
      case "roster":
        return callEvents.roster({
          atMs: reading.atMs,
          participants: reading.participants.map(callEvents.rosterParticipant),
        });
    }
    // A reading from a newer page hook than this content script.
    return undefined;
  }

  function onPageMessage(message) {
    if (message?.t === "hook_state") {
      const channelsOpenedAtMs = message.caption_channels_opened_at;
      follow(captionSource.decide({ kind: "hook_state", channelsOpenedAtMs }));
    } else if (message?.t === "reading") {
      const event = eventFromReading(message.reading);
      if (event) onReaderEvent("channel", event);
    } else if (message?.t === "notice") {
      onPageNotice(message.notice);
    } else if (message?.t === "page_snapshot") {
      for (const sessionId of [...pendingSnapshots.keys()]) finishSnapshot(sessionId, message.page);
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
    // While the leave grace runs, the call controls are gone and would read as captions off.
    if (!call || !isInCall) return;
    if (nowMs - call.last_seen_ms >= 5000) storeSession();

    const decision = captionSource.decide({ kind: "tick", atMs: nowMs });
    follow(decision);
    if (decision.isCaptionsRequestDue) requestCaptions(nowMs);

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
