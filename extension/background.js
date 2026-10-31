// Service worker. It numbers call events, keeps each one in chrome.storage.local until the app
// acks it, and holds the native messaging port to the app. Chrome may stop this worker at any
// time, so every piece of state that matters lives in storage. session-rules.js decides what each
// tab message, tab change and alarm does; this file reads and writes storage and runs the effects.
// It also shows each Meet tab's call status on the toolbar icon. The app decides that status;
// the worker decides only that the app is not reachable.
//
// chrome.storage.local keys: one per field of the session state in session-rules.js, plus
//   q:<seq>       an event message waiting for its ack
//   settings      the app's [meet] settings, which main.js reads
//   reloaded_for  the folder version mc-meet last reloaded itself for
// chrome.storage.session keys, which the popup reads:
//   call_status   the app's last call_status message
//   app           { is_reachable, app_version, checked_at_ms }
// tab-video.js owns the Meet tab video capture and its own chrome.storage.session keys.

import { decide, hasOpenSessions, restoreState, STATE_KEYS } from "./session-rules.js";
import { createTabVideo, NEEDS_CLICK_KEY, VIDEO_CAPTURE_KEY } from "./tab-video.js";

const HOST_NAME = "com.meetingcapture.app";
const PROTOCOL = 1;
const BACKOFF_MIN_MS = 1000;
const BACKOFF_MAX_MS = 60000;
const RECONNECT_ALARM = "mc-meet-reconnect";
const PENDING_END_ALARM = "mc-meet-pending-end";
const QUEUE_PREFIX = "q:";
const SETTINGS_KEY = "settings";
const RELOADED_FOR_KEY = "reloaded_for";
const CALL_STATUS_KEY = "call_status";
const APP_KEY = "app";
const APP_UNREACHABLE_NOTICE = "mc-meet-app-unreachable";
const MEET_TABS = "https://meet.google.com/*";
const BADGE_COLOURS = { recording: "#d93025", problem: "#f29900", off: "#5f6368" };

let nativePort = null;
let isWelcomed = false;
let backoffMs = BACKOFF_MIN_MS;
let reconnectTimer = null;
// The folder version to reload for once no call is open, or null.
let pendingReloadFor = null;
// Open popups. Each keeps the native port open, so it shows the app's live state.
const popupPorts = new Set();
// The content script port of each tab, for snapshot requests.
const tabPorts = new Map();
// The pages_away list last sent on the current native port, as JSON, or null after a connect.
let sentPagesAway = null;

// Every storage change and native send runs on this chain, so seq order holds on the wire.
let chain = Promise.resolve();
function serially(task) {
  const run = chain.then(task);
  chain = run.catch((error) => console.error("mc-meet:", error));
  return run;
}

// The worker's only storage access. A Map-backed object with the same four methods can stand in
// for chrome.storage.local.
const store = {
  readState: () => chrome.storage.local.get(STATE_KEYS),

  // The state and its queued events land in one write, so a stop between them cannot lose an
  // event or store it twice.
  async commit(state, queued) {
    const messages = Object.fromEntries(queued.map((m) => [`${QUEUE_PREFIX}${m.seq}`, m]));
    await chrome.storage.local.set({ ...state, ...messages });
  },

  // Returns the acked message, or undefined when it was already gone.
  async removeAcked(seq) {
    const key = `${QUEUE_PREFIX}${seq}`;
    const acked = (await chrome.storage.local.get(key))[key];
    await chrome.storage.local.remove(key);
    return acked;
  },

  async unacked() {
    const stored = await chrome.storage.local.get(null);
    return Object.entries(stored)
      .filter(([key]) => key.startsWith(QUEUE_PREFIX))
      .map(([, message]) => message)
      .sort((a, b) => a.seq - b.seq);
  },
};

// Applies one input to the stored state. Runs on the chain. port is the tab's port, if any.
async function apply(input, port) {
  const nowMs = Date.now();
  const state = restoreState(await store.readState(), nowMs);
  const decision = decide(state, input, nowMs);
  if (decision.state !== state) await store.commit(decision.state, decision.queued);
  for (const message of decision.queued) {
    if (nativePort && isWelcomed) nativePort.postMessage(message);
    else ensureNative();
  }
  for (const effect of decision.effects) await runEffect(effect, port);
  if (decision.state !== state) sendPagesAway(decision.state);
  if (JSON.stringify(decision.state.tabSessions) !== JSON.stringify(state.tabSessions)) {
    tabVideo.reconcile();
  }
}

// Tells the app which sessions wait for their tab to come back, so it does not take a closed or
// reloaded tab for a call that stopped recording.
function sendPagesAway(state) {
  if (!nativePort || !isWelcomed) return;
  const sessionIds = Object.keys(state.pendingEnds).sort();
  const key = JSON.stringify(sessionIds);
  if (key === sentPagesAway) return;
  sentPagesAway = key;
  nativePort.postMessage({ type: "pages_away", session_ids: sessionIds });
}

async function runEffect(effect, port) {
  switch (effect.kind) {
    case "stored":
      tellTab(port, { type: "stored", session_id: effect.sessionId, n: effect.n });
      return;
    case "session_ended":
      tellTab(port, { type: "session_ended", session_id: effect.sessionId });
      return;
    case "set_discardable":
      chrome.tabs
        .update(Number(effect.tabId), { autoDiscardable: effect.isDiscardable })
        .catch(() => {});
      return;
    case "set_pending_alarm":
      await chrome.alarms.create(PENDING_END_ALARM, { when: effect.whenMs });
      return;
    case "clear_pending_alarm":
      await chrome.alarms.clear(PENDING_END_ALARM);
      return;
  }
}

function tellTab(port, message) {
  try {
    port.postMessage(message);
  } catch {}
}

// ---- Content script ports.

chrome.runtime.onConnect.addListener((port) => {
  if (port.name === "mc-meet-popup") {
    // Only the popup page may stop or resume recording. A content script has a tab.
    const popupPrefix = chrome.runtime.getURL("popup/");
    if (
      port.sender?.id !== chrome.runtime.id ||
      port.sender.tab ||
      !port.sender.url?.startsWith(popupPrefix)
    ) {
      port.disconnect();
      return;
    }
    connectPopup(port);
    return;
  }
  if (port.name !== "mc-meet-tab") return;
  const tabId = port.sender?.tab?.id;
  if (tabId !== undefined) tabPorts.set(tabId, port);
  port.onMessage.addListener((message) => {
    if (tabId === undefined) return;
    // Debug material: sent when the app listens, dropped otherwise.
    if (message?.type === "frame_failed" || message?.type === "page_snapshot") {
      if (nativePort && isWelcomed) nativePort.postMessage(message);
      return;
    }
    serially(async () => {
      await apply({ kind: "tab_message", tabId, message }, port);
      if (message?.type === "resume" || message?.event?.kind === "call_started") {
        await showStatusOnTabs();
      }
    });
  });
  port.onDisconnect.addListener(() => {
    void chrome.runtime.lastError;
    if (tabId === undefined) return;
    if (tabPorts.get(tabId) === port) tabPorts.delete(tabId);
    serially(() => apply({ kind: "tab_gone", tabId }));
  });
});

chrome.tabs.onRemoved.addListener((tabId) => {
  tabPorts.delete(tabId);
  serially(() => apply({ kind: "tab_closed", tabId }));
});

// Chrome may reset a tab's icon when the tab loads a page.
chrome.tabs.onUpdated.addListener((_tabId, change, tab) => {
  if (change.status === "complete" && tab.url?.startsWith("https://meet.google.com/")) {
    serially(showStatusOnTabs);
  }
});

// ---- Toolbar icon and popup.

async function readAppReachability() {
  return (await chrome.storage.session.get(APP_KEY))[APP_KEY] ?? { is_reachable: true };
}

async function noteAppReachable(isReachable, appVersion) {
  const previous = await readAppReachability();
  const app = {
    is_reachable: isReachable,
    app_version: appVersion ?? previous.app_version ?? null,
    checked_at_ms: Date.now(),
  };
  await chrome.storage.session.set({ [APP_KEY]: app });
  if (isReachable) {
    await chrome.notifications.clear(APP_UNREACHABLE_NOTICE);
  } else if (previous.is_reachable !== false && (await hasWork())) {
    // Once per outage, and only with a call to lose.
    chrome.notifications.create(APP_UNREACHABLE_NOTICE, {
      type: "basic",
      iconUrl: "icons/off-32.png",
      title: "Meeting Capture is not running",
      message: "mc-meet keeps this call's events until the app starts.",
    });
  }
  await showStatusOnTabs();
}

// What a call problem keeps from the record, by the app's problem reason code.
function notRecordedOf(reason) {
  switch (reason) {
    case "captions_off":
    case "meet_captions_unavailable":
    case "caption_parse_failures":
    case "captions_silent":
    case "no_captions":
    case "huddle_captions_unreadable":
      return "captions";
    case "extension_disconnected":
    case "meet_page_away":
      return "the call's events";
  }
  // A code from a newer app.
  return "the call's events";
}

// The look of one tab: { icon, badge, title }.
function lookOf(callStatus, isAppReachable, isVideoClickNeeded, isVideoRecording) {
  if (!isAppReachable) {
    return { icon: "off", badge: "OFF", title: "Meeting Capture is not running" };
  }
  if (!callStatus) return { icon: "idle", badge: "", title: "mc-meet: connected" };
  const isCallLive = ["waiting", "recording", "problem"].includes(callStatus.state);
  if (isVideoClickNeeded && isCallLive) {
    return {
      icon: "problem",
      badge: "REC?",
      title: "Click to record this tab's video",
    };
  }
  switch (callStatus.state) {
    case "recording":
      return {
        icon: "recording",
        badge: "REC",
        title: `Recording since ${callStatus.recording_since_label ?? callStatus.started_at_label}`,
      };
    case "problem": {
      // A bare "!" would hide that the tab's video still records.
      if (isVideoRecording) {
        const what = notRecordedOf(callStatus.reason);
        return {
          icon: "problem",
          badge: "REC!",
          title: `Recording video; not recording ${what}: ${callStatus.reason_text}`,
        };
      }
      return { icon: "problem", badge: "!", title: `Not recording: ${callStatus.reason_text}` };
    }
    case "waiting":
      return { icon: "idle", badge: "…", title: "Waiting for captions" };
    case "stopped":
      return { icon: "idle", badge: "", title: "Recording stopped" };
  }
  return { icon: "idle", badge: "…", title: "Waiting for captions" };
}

// Also tells each tab whether the app has its call stopped, so the tab stops or resumes its
// captions and chat. The app's status is the only record of a stop, so a reloaded tab or a
// restarted worker learns it again here.
async function showStatusOnTabs() {
  const { tabSessions } = restoreState(await store.readState(), Date.now());
  const callStatus = (await chrome.storage.session.get(CALL_STATUS_KEY))[CALL_STATUS_KEY];
  const { is_reachable: isAppReachable } = await readAppReachability();
  const needsClick = (await chrome.storage.session.get(NEEDS_CLICK_KEY))[NEEDS_CLICK_KEY] ?? {};
  const videoCapture = (await chrome.storage.session.get(VIDEO_CAPTURE_KEY))[VIDEO_CAPTURE_KEY];
  const tabs = await chrome.tabs.query({ url: MEET_TABS });
  for (const tab of tabs) {
    const sessionId = tabSessions[tab.id];
    const status = sessionId
      ? (callStatus?.calls.find((call) => call.session_ids.includes(sessionId)) ?? {
          state: "waiting",
        })
      : undefined;
    if (sessionId && callStatus) {
      tellTab(tabPorts.get(tab.id), {
        type: "recording_state",
        session_id: sessionId,
        is_stopped: status.state === "stopped",
      });
    }
    const look = lookOf(
      status,
      isAppReachable,
      tab.id in needsClick,
      videoCapture?.tab_id === tab.id,
    );
    const tabId = tab.id;
    await chrome.action
      .setIcon({
        tabId,
        path: { 16: `icons/${look.icon}-16.png`, 32: `icons/${look.icon}-32.png` },
      })
      .catch(() => {});
    await chrome.action.setBadgeText({ tabId, text: look.badge }).catch(() => {});
    const colour = BADGE_COLOURS[look.icon];
    if (colour)
      await chrome.action.setBadgeBackgroundColor({ tabId, color: colour }).catch(() => {});
    await chrome.action.setTitle({ tabId, title: look.title }).catch(() => {});
  }
}

function connectPopup(port) {
  popupPorts.add(port);
  ensureNative();
  port.onMessage.addListener((message) => {
    if (message?.type === "popup_opened") {
      // Opening the popup invokes mc-meet on the tab, which lets it capture the tab.
      tabVideo.onInvoked(message.tab_id);
      return;
    }
    if (message?.type === "stop_recording" || message?.type === "resume_recording") {
      serially(async () => {
        if (!nativePort || !isWelcomed) return;
        const { tabSessions } = restoreState(await store.readState(), Date.now());
        const sessionId = tabSessions[message.tab_id];
        if (sessionId) nativePort.postMessage({ type: message.type, session_id: sessionId });
      });
      return;
    }
    if (message?.type !== "save_debug_report") return;
    serially(async () => {
      if (!nativePort || !isWelcomed) {
        tellTab(port, {
          type: "debug_report_failed",
          error: "Meeting Capture is not running, or is still connecting.",
        });
        return;
      }
      const { tabSessions } = restoreState(await store.readState(), Date.now());
      nativePort.postMessage({
        type: "save_debug_report",
        session_id: tabSessions[message.tab_id] ?? null,
        with_text: message.with_text === true,
      });
    });
  });
  port.onDisconnect.addListener(() => {
    popupPorts.delete(port);
    serially(closeNativeIfIdle);
  });
}

function tellPopups(message) {
  for (const port of popupPorts) tellTab(port, message);
}

async function storeCallStatus(callStatus) {
  await chrome.storage.session.set({ [CALL_STATUS_KEY]: callStatus });
  await showStatusOnTabs();
  tabVideo.noteCallStatus();
}

async function requestSnapshots(sessionIds) {
  const { tabSessions } = restoreState(await store.readState(), Date.now());
  for (const [tabId, sessionId] of Object.entries(tabSessions)) {
    if (!sessionIds.includes(sessionId)) continue;
    tellTab(tabPorts.get(Number(tabId)), { type: "snapshot_request", session_id: sessionId });
  }
}

// ---- Native port to the app.

async function hasWork() {
  if (popupPorts.size > 0) return true;
  if ((await store.unacked()).length > 0) return true;
  return hasOpenSessions(restoreState(await store.readState(), Date.now()));
}

// The sessions this worker holds open, for hello: the app then knows this port carries them.
async function openSessionIds() {
  const { tabSessions, pendingEnds } = restoreState(await store.readState(), Date.now());
  return [...new Set([...Object.values(tabSessions), ...Object.keys(pendingEnds)])];
}

async function resendUnacked() {
  for (const message of await store.unacked()) nativePort?.postMessage(message);
}

async function onAck(seq) {
  const acked = await store.removeAcked(seq);
  // After the last call ends and nothing is left to send, close the port so the host can exit.
  // Only a call_ended ack can make the worker idle, so only that ack pays for the full check.
  if (acked?.event?.kind !== "call_ended") return;
  await showStatusOnTabs();
  await closeNativeIfIdle();
}

async function closeNativeIfIdle() {
  if (nativePort && !(await hasWork())) {
    nativePort.disconnect();
    nativePort = null;
    isWelcomed = false;
    // The status names calls of other profiles too. Kept past the port, a press here would
    // read them as this profile's.
    await chrome.storage.session.remove(CALL_STATUS_KEY);
    await reloadIfPending();
  }
}

// The app rewrites mc-meet's folder, stamped with its own version, when that version changes, and
// sends the folder's version in welcome. Chrome reads the new files only on a reload. A reload in
// a call would orphan the Meet tab's content scripts, so it waits for the worker to go idle. It
// runs once per folder version, so a folder the app does not manage, such as a clone, does not
// reload after every call.
async function noteFolderVersion(folderVersion) {
  if (!folderVersion || folderVersion === chrome.runtime.getManifest().version) return;
  const reloadedFor = (await chrome.storage.local.get(RELOADED_FOR_KEY))[RELOADED_FOR_KEY];
  if (reloadedFor !== folderVersion) pendingReloadFor = folderVersion;
}

async function reloadIfPending() {
  if (pendingReloadFor === null) return;
  console.info("mc-meet: reloading for folder version", pendingReloadFor);
  await chrome.storage.local.set({ [RELOADED_FOR_KEY]: pendingReloadFor });
  chrome.runtime.reload();
}

function scheduleReconnect() {
  if (reconnectTimer !== null) return;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    ensureNative();
  }, backoffMs);
  backoffMs = Math.min(backoffMs * 2, BACKOFF_MAX_MS);
}

function dropNative(port) {
  if (nativePort !== port) return;
  nativePort = null;
  isWelcomed = false;
  scheduleReconnect();
}

function connectNative(sessions) {
  let port;
  try {
    port = chrome.runtime.connectNative(HOST_NAME);
  } catch (error) {
    console.warn("mc-meet: connectNative failed:", error);
    serially(() => noteAppReachable(false));
    scheduleReconnect();
    return;
  }
  nativePort = port;
  isWelcomed = false;
  port.onMessage.addListener((message) => {
    if (message?.type === "welcome") {
      isWelcomed = true;
      sentPagesAway = null;
      backoffMs = BACKOFF_MIN_MS;
      console.info("mc-meet: app", message.app_version, "open sessions", message.open_sessions);
      storeSettings(message.settings);
      serially(() => noteAppReachable(true, message.app_version));
      serially(() => noteFolderVersion(message.folder_version));
      serially(resendUnacked);
      serially(async () => sendPagesAway(restoreState(await store.readState(), Date.now())));
      tabVideo.onAppWelcomed();
      tabVideo.reconcile();
      // A restarted app has no health for the calls; each tab resends its own.
      for (const tabPort of tabPorts.values()) tellTab(tabPort, { type: "app_connected" });
      tellPopups({ type: "app_changed" });
    } else if (message?.type === "settings") {
      storeSettings(message.settings);
    } else if (message?.type === "ack") {
      serially(() => onAck(message.seq));
    } else if (message?.type === "call_status") {
      serially(() => storeCallStatus({ calls: message.calls ?? [] }));
    } else if (message?.type === "shortcut_press_expected") {
      tabVideo.onPressExpected(message.session_id);
    } else if (message?.type === "snapshot_request") {
      serially(() => requestSnapshots(message.session_ids ?? []));
    } else if (message?.type === "debug_report_saved" || message?.type === "debug_report_failed") {
      tellPopups(message);
    } else if (message?.type === "app_unavailable") {
      console.warn("mc-meet: the app did not start");
      port.disconnect();
      dropNative(port);
      serially(() => noteAppReachable(false));
      tellPopups({ type: "app_changed" });
    }
  });
  port.onDisconnect.addListener(() => {
    const error = chrome.runtime.lastError?.message;
    console.warn("mc-meet: native port closed:", error ?? "no error");
    // A port closed before welcome means the host or the app is missing.
    if (nativePort === port && !isWelcomed) serially(() => noteAppReachable(false));
    dropNative(port);
    tellPopups({ type: "app_changed" });
  });
  port.postMessage({
    type: "hello",
    protocol: PROTOCOL,
    extension_version: chrome.runtime.getManifest().version,
    sessions,
  });
}

// An older app sends no settings; the stored ones stay.
function storeSettings(settings) {
  if (!settings) return;
  chrome.storage.local.set({ [SETTINGS_KEY]: settings }).catch((error) => {
    console.error("mc-meet: could not store the settings:", error);
  });
}

// Video messages from tab-video.js. They are not queued in storage: a lost fragment costs two
// seconds of video, and tab-video.js holds a few in memory while the port is down.
function sendToApp(message) {
  if (nativePort && isWelcomed) {
    nativePort.postMessage(message);
    return true;
  }
  ensureNative();
  return false;
}

// Its listeners must register in the worker's first run, as this file's do.
const tabVideo = createTabVideo({
  sendToApp,
  isAppWelcomed: () => nativePort !== null && isWelcomed,
  refreshLook: () => serially(showStatusOnTabs),
  readTabSessions: async () => restoreState(await store.readState(), Date.now()).tabSessions,
});

function ensureNative() {
  if (nativePort || reconnectTimer !== null) return;
  serially(async () => {
    if (!nativePort && (await hasWork())) connectNative(await openSessionIds());
  });
}

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === PENDING_END_ALARM) serially(() => apply({ kind: "pending_due" }));
  if (alarm.name === RECONNECT_ALARM) ensureNative();
});

// Sessions whose tab no longer exists wait for a restored tab to resume them.
async function holdSessionsOfMissingTabs() {
  const { tabSessions } = restoreState(await store.readState(), Date.now());
  const tabIds = [];
  for (const tabId of Object.keys(tabSessions)) {
    const tab = await chrome.tabs.get(Number(tabId)).catch(() => undefined);
    if (!tab) tabIds.push(tabId);
  }
  await apply({ kind: "tabs_missing", tabIds });
}

// An update or reload orphans the content scripts in open Meet tabs. Inject fresh ones; they
// resume the tab's session from sessionStorage. The page hook from the old version keeps running.
chrome.runtime.onInstalled.addListener(async (details) => {
  if (details.reason !== "update") return;
  const files = chrome.runtime.getManifest().content_scripts.find((c) => c.world !== "MAIN").js;
  const tabs = await chrome.tabs.query({ url: "https://meet.google.com/*" });
  for (const tab of tabs) {
    chrome.scripting.executeScript({ target: { tabId: tab.id }, files }).catch(() => {});
  }
});

chrome.alarms.create(RECONNECT_ALARM, { periodInMinutes: 0.5 });
serially(holdSessionsOfMissingTabs);
serially(showStatusOnTabs);
tabVideo.reconcile();
ensureNative();
