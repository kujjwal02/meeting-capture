// Meet tab video: the worker side. It decides which Meet tab to capture from the app's
// is_video_wanted flags, opens the offscreen document that records it, and forwards the
// document's fragments and states to the app. It owns one capture at a time.
//
// Chrome hands out a tab's stream id only after the user invoked mc-meet on that tab (toolbar
// click, popup, context menu or the shortcut). Until then the tab sits in video_needs_click and
// the toolbar asks for a click.
//
// chrome.storage.session keys, which the popup reads:
//   video_capture      { tab_id, session_id, capture_id } of the running capture, or null
//   video_needs_click  { [tab_id]: session_id } for tabs that need an invocation
//   video_noticed      { [tab_id]: session_id } for tabs whose notification has shown

export const VIDEO_CAPTURE_KEY = "video_capture";
export const NEEDS_CLICK_KEY = "video_needs_click";
const NOTICED_KEY = "video_noticed";
const CALL_STATUS_KEY = "call_status";
const COMMAND = "toggle-recording";
const MENU_ID = "mc-meet-record-tab";
const OFFSCREEN_PATH = "offscreen/offscreen.html";
const MEET_PREFIX = "https://meet.google.com/";
const MAX_BUFFERED_FRAGMENTS = 30;
// A session that fails to start this many times in a row waits for an invocation or a new want.
const MAX_START_ATTEMPTS = 2;
// The app sends call_status at least every 5 s while it wants video. Without one for this long,
// capture stops: a stop the app sent may be lost, and nothing would pair with the frames.
const APP_SILENCE_LIMIT_MS = 10_000;

// sendToApp(message) posts on the native port and returns false when the port is not welcomed.
// refreshLook() redraws the toolbar; it must not wait on this module.
// readTabSessions() resolves to the worker's { [tabId]: session_id }.
export function createTabVideo({ sendToApp, refreshLook, readTabSessions }) {
  // Messages for the app while its port is not welcomed, oldest first.
  const pendingForApp = [];
  // The last { state, detail, captureId } sent per session, so each change goes out once. The
  // capture id keeps a second capture's "stopped" from looking like the first one's.
  // An app that restarted or a call whose video is wanted again must hear a repeated state, so
  // a welcome clears the map and a session that turns wanted clears its entry.
  const sentStates = new Map();
  let lastWantedSessionIds = new Set();
  const startAttemptsBySession = new Map();
  // Tabs the user invoked mc-meet on since this worker started.
  const invokedTabIds = new Set();
  // Monotonic, so a Mac sleep does not count as app silence. A restarted worker gives the app a
  // full limit.
  let lastCallStatusMonoMs = performance.now();

  let chain = Promise.resolve();
  function serially(task) {
    const run = chain.then(task);
    chain = run.catch((error) => console.error("mc-meet video:", error));
    return run;
  }

  function forwardToApp(message) {
    if (pendingForApp.length === 0 && sendToApp(message)) return;
    pendingForApp.push(message);
    const fragmentCount = pendingForApp.filter((m) => m.type === "video_fragment").length;
    if (fragmentCount > MAX_BUFFERED_FRAGMENTS) {
      pendingForApp.splice(
        pendingForApp.findIndex((m) => m.type === "video_fragment"),
        1,
      );
      console.warn("mc-meet video: app not connected, dropped the oldest of", fragmentCount);
    }
    flushBuffered();
  }

  function flushBuffered() {
    while (pendingForApp.length > 0 && sendToApp(pendingForApp[0])) pendingForApp.shift();
  }

  // ended: { capture_id, last_index } of the capture a "stopped" ends, so the app knows which
  // fragment is the last.
  function sendState(sessionId, state, detail, ended = null) {
    const captureId = ended?.capture_id ?? null;
    const previous = sentStates.get(sessionId);
    if (
      previous?.state === state &&
      previous.detail === detail &&
      previous.captureId === captureId
    ) {
      return;
    }
    sentStates.set(sessionId, { state, detail, captureId });
    console.info("mc-meet video:", sessionId, state, detail ?? "", ended ?? "");
    forwardToApp({ type: "video_state", session_id: sessionId, state, detail, ...ended });
  }

  async function readSession() {
    const stored = await chrome.storage.session.get([
      CALL_STATUS_KEY,
      VIDEO_CAPTURE_KEY,
      NEEDS_CLICK_KEY,
      NOTICED_KEY,
    ]);
    return {
      calls: stored[CALL_STATUS_KEY]?.calls ?? [],
      running: stored[VIDEO_CAPTURE_KEY] ?? null,
      needsClick: stored[NEEDS_CLICK_KEY] ?? {},
      noticed: stored[NOTICED_KEY] ?? {},
    };
  }

  async function askOffscreen(message) {
    if (!(await chrome.offscreen.hasDocument())) return undefined;
    return chrome.runtime.sendMessage({ target: "offscreen", ...message });
  }

  async function ensureOffscreen() {
    if (await chrome.offscreen.hasDocument()) return;
    await chrome.offscreen.createDocument({
      url: OFFSCREEN_PATH,
      reasons: ["USER_MEDIA", "AUDIO_PLAYBACK"],
      justification: "Record the Meet tab's video and play its audio back while it is captured.",
    });
  }

  async function closeOffscreen() {
    if (await chrome.offscreen.hasDocument()) await chrome.offscreen.closeDocument();
  }

  // The stored capture, if the offscreen document still runs it. A restarted worker or a closed
  // document can leave either side stale.
  async function readLiveCapture(running) {
    const status = await askOffscreen({ type: "status" }).catch(() => undefined);
    const liveId = status?.capture_id ?? null;
    if (running && liveId === running.capture_id) return running;
    if (liveId !== null) {
      const reply = await askOffscreen({ type: "stop", reason: "not_wanted" }).catch(() => {});
      sendState(status.session_id, "stopped", "not_wanted", reply?.ended ?? null);
    }
    if (running) await chrome.storage.session.set({ [VIDEO_CAPTURE_KEY]: null });
    await closeOffscreen().catch(() => {});
    return null;
  }

  async function stopCapture(running, reason) {
    const reply = await askOffscreen({ type: "stop", reason }).catch(() => {});
    sendState(running.session_id, "stopped", reason, reply?.ended ?? null);
    await chrome.storage.session.set({ [VIDEO_CAPTURE_KEY]: null });
    await closeOffscreen().catch(() => {});
  }

  // A notification shown while mc-meet's one captions press was due kept the press from reaching
  // Meet. So the notice waits until the press is over: the call has a caption source, or it has
  // a problem reason, which a call whose caption_source stays "none" gets within about 20 s.
  // The badge shows at once.
  function isCaptionPressOver(call) {
    return (
      call?.caption_source === "channel" ||
      call?.caption_source === "dom" ||
      (call?.state === "problem" && typeof call.reason === "string")
    );
  }

  async function showDueNotices(calls, needsClick, noticed) {
    for (const [tabId, sessionId] of Object.entries(needsClick)) {
      const call = calls.find((candidate) => candidate.session_ids.includes(sessionId));
      if (isCaptionPressOver(call)) await showNotice(tabId, sessionId, noticed);
    }
  }

  async function showNotice(tabId, sessionId, noticed) {
    if (noticed[tabId] === sessionId) return;
    noticed[tabId] = sessionId;
    await chrome.storage.session.set({ [NOTICED_KEY]: { ...noticed } });
    chrome.notifications.create(`mc-meet-video-${tabId}`, {
      type: "basic",
      iconUrl: "icons/problem-32.png",
      title: "Meet video is not recording",
      message:
        'Click the mc-meet icon on the Meet tab, or its "Record this tab\'s video" menu entry, to record its video.',
    });
  }

  async function start(tabId, sessionId, needsClick) {
    startAttemptsBySession.set(sessionId, (startAttemptsBySession.get(sessionId) ?? 0) + 1);
    let streamId;
    try {
      streamId = await chrome.tabCapture.getMediaStreamId({ targetTabId: tabId });
    } catch (error) {
      const text = String(error?.message ?? error);
      // Without a grant Chrome says "Extension has not been invoked for the current page (see
      // activeTab permission). Chrome pages cannot be captured." Any other error after an
      // invocation is a real failure.
      if (!text.includes("invoked") && invokedTabIds.has(tabId)) {
        sendState(sessionId, "failed", text);
        return;
      }
      await chrome.storage.session.set({
        [NEEDS_CLICK_KEY]: { ...needsClick, [tabId]: sessionId },
      });
      sendState(sessionId, "needs_click", null);
      refreshLook();
      return;
    }
    await ensureOffscreen();
    const captureId = Array.from(crypto.getRandomValues(new Uint8Array(8)), (byte) =>
      byte.toString(16).padStart(2, "0"),
    ).join("");
    const reply = await chrome.runtime
      .sendMessage({
        target: "offscreen",
        type: "start",
        stream_id: streamId,
        session_id: sessionId,
        capture_id: captureId,
      })
      .catch((error) => ({ ok: false, error: String(error?.message ?? error) }));
    if (!reply?.ok) {
      sendState(sessionId, "failed", reply?.error ?? "the offscreen document did not answer");
      await closeOffscreen().catch(() => {});
      return;
    }
    await chrome.storage.session.set({
      [VIDEO_CAPTURE_KEY]: { tab_id: tabId, session_id: sessionId, capture_id: captureId },
    });
    refreshLook();
  }

  // Among tabs whose call wants video, the active one, else the highest tab id.
  async function pickWantedTab(calls, tabSessions) {
    const wantedSessionIds = new Set(
      calls.filter((call) => call.is_video_wanted === true).flatMap((call) => call.session_ids),
    );
    const tabs = [];
    for (const [tabId, sessionId] of Object.entries(tabSessions)) {
      if (!wantedSessionIds.has(sessionId)) continue;
      const tab = await chrome.tabs.get(Number(tabId)).catch(() => undefined);
      if (tab?.url?.startsWith(MEET_PREFIX)) tabs.push(tab);
    }
    tabs.sort((a, b) => Number(b.active) - Number(a.active) || b.id - a.id);
    return { wantedSessionIds, wantedTab: tabs[0] };
  }

  function isAppSilent() {
    return performance.now() - lastCallStatusMonoMs > APP_SILENCE_LIMIT_MS;
  }

  async function reconcileNow() {
    const { calls, running: stored, needsClick, noticed } = await readSession();
    const tabSessions = await readTabSessions();
    const { wantedSessionIds, wantedTab } = await pickWantedTab(calls, tabSessions);

    for (const sessionId of wantedSessionIds) {
      if (!lastWantedSessionIds.has(sessionId)) sentStates.delete(sessionId);
    }
    lastWantedSessionIds = wantedSessionIds;
    for (const sessionId of startAttemptsBySession.keys()) {
      if (!wantedSessionIds.has(sessionId)) startAttemptsBySession.delete(sessionId);
    }
    const keptNeedsClick = Object.fromEntries(
      Object.entries(needsClick).filter(
        ([tabId, sessionId]) => wantedSessionIds.has(sessionId) && tabSessions[tabId] === sessionId,
      ),
    );
    if (Object.keys(keptNeedsClick).length !== Object.keys(needsClick).length) {
      await chrome.storage.session.set({ [NEEDS_CLICK_KEY]: keptNeedsClick });
      refreshLook();
    }
    await showDueNotices(calls, keptNeedsClick, noticed);

    let running = await readLiveCapture(stored);
    const wantedSessionId = wantedTab ? tabSessions[wantedTab.id] : undefined;
    const isRunningWanted =
      running?.tab_id === wantedTab?.id && running?.session_id === wantedSessionId;
    if (running && (!isRunningWanted || isAppSilent())) {
      await stopCapture(running, isAppSilent() ? "app_silent" : "not_wanted");
      running = null;
      refreshLook();
    }
    if (!wantedTab || running || wantedTab.id in keptNeedsClick || isAppSilent()) return;
    if ((startAttemptsBySession.get(wantedSessionId) ?? 0) >= MAX_START_ATTEMPTS) return;
    await start(wantedTab.id, wantedSessionId, keptNeedsClick);
  }

  function reconcile() {
    return serially(reconcileNow);
  }

  // Each call_status from the app, including the unchanged ones it repeats while it wants video.
  function noteCallStatus() {
    lastCallStatusMonoMs = performance.now();
    return reconcile();
  }

  // Fragments arrive every 2 s and wake the worker, so the silence check runs on them.
  function checkAppSilence() {
    if (!isAppSilent()) return;
    serially(async () => {
      const { running } = await readSession();
      if (!running || !isAppSilent()) return;
      console.warn("mc-meet video: no call_status from the app, stopping the capture");
      await stopCapture(running, "app_silent");
      refreshLook();
    });
  }

  // The user invoked mc-meet on the tab, which grants its stream id for the tab's life.
  function onInvoked(tabId) {
    if (!Number.isInteger(tabId)) return Promise.resolve();
    return serially(async () => {
      invokedTabIds.add(tabId);
      const { needsClick } = await readSession();
      const sessionId = (await readTabSessions())[tabId];
      if (sessionId) startAttemptsBySession.delete(sessionId);
      if (tabId in needsClick) {
        const { [tabId]: _removed, ...rest } = needsClick;
        await chrome.storage.session.set({ [NEEDS_CLICK_KEY]: rest });
        chrome.notifications.clear(`mc-meet-video-${tabId}`).catch(() => {});
        refreshLook();
      }
      await reconcileNow();
    });
  }

  // The shortcut is the user's stop, so it always stops a recording call, whatever its video
  // does. The press also grants the tab, so a resume can capture it with no click. The toolbar
  // click and the context menu retry the video.
  function isCallRecording(call) {
    return call?.state === "waiting" || call?.state === "recording" || call?.state === "problem";
  }

  // Each welcome is a fresh app or a fresh port, which may not have seen the last states.
  function onAppWelcomed() {
    sentStates.clear();
    flushBuffered();
  }

  function onShortcut(tabId) {
    if (!Number.isInteger(tabId)) return Promise.resolve();
    return serially(async () => {
      const { calls } = await readSession();
      const sessionId = (await readTabSessions())[tabId];
      invokedTabIds.add(tabId);
      // A press away from Meet stops whatever records, as a press on the Meet tab does.
      if (!sessionId) {
        const recording = calls.filter(isCallRecording);
        for (const call of recording) {
          forwardToApp({ type: "stop_recording", session_id: call.session_ids[0] });
        }
        if (recording.length === 0) onInvoked(tabId);
        return;
      }
      const call = calls.find((candidate) => candidate.session_ids.includes(sessionId));
      if (isCallRecording(call)) {
        forwardToApp({ type: "stop_recording", session_id: sessionId });
        return;
      }
      if (call?.state === "stopped") {
        forwardToApp({ type: "resume_recording", session_id: sessionId });
        return;
      }
      onInvoked(tabId);
    });
  }

  function onCaptureEnded(sessionId) {
    return serially(async () => {
      const { running } = await readSession();
      if (running?.session_id !== sessionId) return;
      await chrome.storage.session.set({ [VIDEO_CAPTURE_KEY]: null });
      await closeOffscreen().catch(() => {});
      refreshLook();
      // The tab's grant may still hold, so one more start can pick the call back up.
      await reconcileNow();
    });
  }

  function onTabGone(tabId) {
    return serially(async () => {
      invokedTabIds.delete(tabId);
      const { running, needsClick, noticed } = await readSession();
      if (running?.tab_id === tabId) {
        await stopCapture(running, "tab_gone");
        refreshLook();
      }
      if (tabId in needsClick || tabId in noticed) {
        const { [tabId]: _clicked, ...keptNeedsClick } = needsClick;
        const { [tabId]: _noticed, ...keptNoticed } = noticed;
        await chrome.storage.session.set({
          [NEEDS_CLICK_KEY]: keptNeedsClick,
          [NOTICED_KEY]: keptNoticed,
        });
      }
    });
  }

  function takeOffscreenMessage(message, sender) {
    const offscreenUrl = chrome.runtime.getURL(OFFSCREEN_PATH);
    if (sender.id !== chrome.runtime.id || sender.url !== offscreenUrl) return;
    const { target: _target, ...wire } = message;
    if (wire.type === "video_fragment") {
      forwardToApp(wire);
      checkAppSilence();
      return;
    }
    if (wire.type !== "video_state") return;
    switch (wire.state) {
      case "capturing":
        startAttemptsBySession.delete(wire.session_id);
        sendState(wire.session_id, wire.state, wire.detail);
        return;
      case "stopped":
        sendState(wire.session_id, wire.state, wire.detail, {
          capture_id: wire.capture_id,
          last_index: wire.last_index,
        });
        onCaptureEnded(wire.session_id);
        return;
      case "failed":
        sendState(wire.session_id, wire.state, wire.detail);
        onCaptureEnded(wire.session_id);
        return;
      case "needs_click":
        return;
    }
  }

  chrome.runtime.onMessage.addListener((message, sender) => {
    if (message?.target === "worker") takeOffscreenMessage(message, sender);
  });

  chrome.runtime.onInstalled.addListener(async () => {
    await chrome.contextMenus.removeAll();
    chrome.contextMenus.create(
      {
        id: MENU_ID,
        title: "Record this tab's video",
        contexts: ["action", "page"],
        documentUrlPatterns: [`${MEET_PREFIX}*`],
      },
      () => void chrome.runtime.lastError,
    );
  });

  chrome.contextMenus.onClicked.addListener((clicked, tab) => {
    if (clicked.menuItemId === MENU_ID) onInvoked(tab?.id);
  });

  chrome.commands.onCommand.addListener(async (command, tab) => {
    if (command !== COMMAND) return;
    const target = tab ?? (await chrome.tabs.query({ active: true, lastFocusedWindow: true }))[0];
    onShortcut(target?.id);
  });

  chrome.tabs.onRemoved.addListener((tabId) => onTabGone(tabId));

  // Without the tabs permission, tab.url is unset once the tab leaves meet.google.com.
  chrome.tabs.onUpdated.addListener((tabId, change, tab) => {
    if (change.url === undefined && change.status !== "loading") return;
    if (!tab.url?.startsWith(MEET_PREFIX)) onTabGone(tabId);
  });

  return { reconcile, noteCallStatus, onInvoked, onAppWelcomed };
}
