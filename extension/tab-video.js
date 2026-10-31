// Meet tab video: the worker side. It decides which Meet tab to capture from the app's
// is_video_wanted flags, opens the offscreen document that records it, and forwards the
// document's fragments and states to the app. It owns one capture at a time.
//
// Chrome hands out a tab's stream id only after an invocation of mc-meet on that tab (toolbar
// click, popup, context menu or the shortcut). Until then the tab sits in video_needs_click and
// the toolbar asks for a click. The app tries first: each needs_click tells it whether the Meet
// tab is the active tab of Chrome's last focused window and which key the shortcut is bound
// to, and the app presses that key into Chrome's process, which takes it even from behind
// another app. A press on a tab in video_needs_click only arms it, whoever pressed, so the
// app's press never stops a recording. Before it presses, the app sends
// shortcut_press_expected; the worker reads the facts again and answers ready or refused, and
// reports a press that never arrives as unseen.
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
// The app presses at most 1 s after its shortcut_press_expected notice, and skips the press
// once that limit passes. Both press windows open at the notice and last 1 s past that limit,
// for the key to arrive. A press missing by then went to a Chrome window this worker cannot
// see, such as another profile.
const PRESS_SEEN_LIMIT_MS = 2_000;

// sendToApp(message) posts on the native port and returns false when the port is not welcomed.
// isAppWelcomed() is true while the native port is open and welcomed.
// refreshLook() redraws the toolbar; it must not wait on this module.
// readTabSessions() resolves to the worker's { [tabId]: session_id }.
export function createTabVideo({ sendToApp, isAppWelcomed, refreshLook, readTabSessions }) {
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
  // A call_status came on the welcomed port. Until then the stored calls are an earlier port's.
  let isCallStatusCurrent = false;
  // The app's press this worker answered ready for: { sessionId, tabId, timer }, until the
  // command arrives or the timer reports it unseen. Memory only: the native port keeps the
  // worker alive for the 1 s it matters.
  let awaitedPress = null;
  // The app's press notice for a session this worker does not hold: { sessionId, untilMs } on
  // performance.now(). Another profile's mc-meet is the target, and a command in this window is
  // the app's press landing here, not the user's.
  let foreignPress = null;

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
  // fragment is the last. facts: the needs_click fields from readPressFacts, which count as a
  // change of their own, so a focus change reaches the app.
  function sendState(sessionId, state, detail, ended = null, facts = null) {
    const captureId = ended?.capture_id ?? null;
    const factsKey = JSON.stringify(facts);
    const previous = sentStates.get(sessionId);
    if (
      previous?.state === state &&
      previous.detail === detail &&
      previous.captureId === captureId &&
      previous.factsKey === factsKey
    ) {
      return;
    }
    sentStates.set(sessionId, { state, detail, captureId, factsKey });
    console.info("mc-meet video:", sessionId, state, detail ?? "", ended ?? "", facts ?? "");
    forwardToApp({
      type: "video_state",
      session_id: sessionId,
      state,
      detail,
      ...ended,
      ...facts,
    });
  }

  // What the app's shortcut press depends on: Chrome gives the invocation to the active tab of
  // its last focused window, and the user may bind the shortcut to another key or to none.
  async function readPressFacts(tabId) {
    const tab = await chrome.tabs.get(tabId).catch(() => undefined);
    const lastFocused = await chrome.windows.getLastFocused().catch(() => undefined);
    const commands = await chrome.commands.getAll().catch(() => []);
    return {
      meet_tab: {
        is_tab_active: tab?.active === true,
        is_window_last_focused: tab !== undefined && lastFocused?.id === tab.windowId,
      },
      shortcut: commands.find((command) => command.name === COMMAND)?.shortcut ?? "",
    };
  }

  async function sendNeedsClick(tabId, sessionId) {
    sendState(sessionId, "needs_click", null, null, await readPressFacts(tabId));
  }

  // The tab focus changed, so the app may be able to press now.
  function onFocusChanged() {
    return serially(async () => {
      const { needsClick } = await readSession();
      for (const [tabId, sessionId] of Object.entries(needsClick)) {
        await sendNeedsClick(Number(tabId), sessionId);
      }
    });
  }

  // The app presses the shortcut once it has the answer. The tab may have moved since its
  // needs_click, so the facts are read again; the new ones go first, then the refusal.
  function onPressExpected(sessionId) {
    return serially(async () => {
      const tabSessions = await readTabSessions();
      const tabEntry = Object.entries(tabSessions).find(([, id]) => id === sessionId);
      // Another profile's worker holds the session. The app presses only after this answer,
      // so the window is open before the key can land here.
      if (!tabEntry) {
        foreignPress = { sessionId, untilMs: performance.now() + PRESS_SEEN_LIMIT_MS };
        sendToApp({ type: "shortcut_press_noted", session_id: sessionId });
        return;
      }
      const tabId = Number(tabEntry[0]);
      const { needsClick } = await readSession();
      const facts = await readPressFacts(tabId);
      const isWherePressLands =
        facts.meet_tab.is_tab_active && facts.meet_tab.is_window_last_focused;
      if (!(tabId in needsClick) || !isWherePressLands) {
        if (tabId in needsClick) sendState(sessionId, "needs_click", null, null, facts);
        sendToApp({ type: "shortcut_press_refused", session_id: sessionId });
        return;
      }
      clearAwaitedPress();
      awaitedPress = {
        sessionId,
        tabId,
        timer: setTimeout(() => onPressUnseen(sessionId), PRESS_SEEN_LIMIT_MS),
      };
      sendToApp({ type: "shortcut_press_ready", session_id: sessionId });
    });
  }

  function onPressUnseen(sessionId) {
    if (awaitedPress?.sessionId !== sessionId) return;
    awaitedPress = null;
    console.warn("mc-meet video: the app's shortcut press never arrived for", sessionId);
    sendToApp({ type: "shortcut_press_unseen", session_id: sessionId, reason: "no_command" });
  }

  // Returns the session of a foreign press notice still in its window, and clears it.
  function takeForeignPress() {
    const foreign = foreignPress;
    foreignPress = null;
    return foreign && performance.now() <= foreign.untilMs ? foreign.sessionId : null;
  }

  // Returns the awaited press, if any, and stops its unseen timer.
  function clearAwaitedPress() {
    const awaited = awaitedPress;
    if (awaited) clearTimeout(awaited.timer);
    awaitedPress = null;
    return awaited;
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
      await sendNeedsClick(tabId, sessionId);
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
    isCallStatusCurrent = true;
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

  // On a tab that needs the invocation, the shortcut only arms it: the app presses it there
  // too. Elsewhere the shortcut is the user's stop, so it stops a recording call, whatever its
  // video does. The press also grants the tab, so a resume can capture it with no click. The
  // toolbar click and the context menu retry the video.
  function isCallRecording(call) {
    return call?.state === "waiting" || call?.state === "recording" || call?.state === "problem";
  }

  // Each welcome is a fresh app or a fresh port, which may not have seen the last states. A
  // press awaited by the old app is moot.
  function onAppWelcomed() {
    isCallStatusCurrent = false;
    sentStates.clear();
    clearAwaitedPress();
    foreignPress = null;
    flushBuffered();
  }

  function onShortcut(tabId) {
    if (!Number.isInteger(tabId)) return Promise.resolve();
    return serially(async () => {
      const { calls, needsClick } = await readSession();
      if (tabId in needsClick) {
        console.info("mc-meet video: the shortcut arms tab", tabId);
        onInvoked(tabId);
        return;
      }
      const sessionId = (await readTabSessions())[tabId];
      invokedTabIds.add(tabId);
      // A worker with no welcomed port heard no press notice, so the press may be the app's
      // for another profile. Its stored calls may also be an earlier port's. A stop sent then
      // would reconnect and end a call this profile does not hold.
      if (!isAppWelcomed() || !isCallStatusCurrent) {
        console.info("mc-meet video: no current call status, the shortcut stops nothing");
        return;
      }
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
    // Taken before any await, so the unseen timer cannot fire for a press that arrived.
    const awaited = clearAwaitedPress();
    const foreignSessionId = takeForeignPress();
    // The app pressed for another profile's Meet tab and the key landed here. Taking it as the
    // user's stop would end a call in this profile, so it does nothing and the app hears where
    // it went.
    if (!awaited && foreignSessionId) {
      console.warn("mc-meet video: the app's press for another profile landed here");
      sendToApp({ type: "shortcut_press_landed", session_id: foreignSessionId });
      return;
    }
    const target = tab ?? (await chrome.tabs.query({ active: true, lastFocusedWindow: true }))[0];
    // The app's press landed on another tab: arming or stopping that tab would be wrong, so the
    // press does nothing and the app hears it as unseen.
    if (awaited && awaited.tabId !== target?.id) {
      console.warn("mc-meet video: the app's press reached tab", target?.id, "not", awaited.tabId);
      sendToApp({
        type: "shortcut_press_unseen",
        session_id: awaited.sessionId,
        reason: "other_tab",
      });
      return;
    }
    onShortcut(target?.id);
  });

  chrome.tabs.onActivated.addListener(() => onFocusChanged());
  chrome.windows.onFocusChanged.addListener(() => onFocusChanged());

  chrome.tabs.onRemoved.addListener((tabId) => onTabGone(tabId));

  // Without the tabs permission, tab.url is unset once the tab leaves meet.google.com.
  chrome.tabs.onUpdated.addListener((tabId, change, tab) => {
    if (change.url === undefined && change.status !== "loading") return;
    if (!tab.url?.startsWith(MEET_PREFIX)) onTabGone(tabId);
  });

  return { reconcile, noteCallStatus, onInvoked, onAppWelcomed, onPressExpected };
}
