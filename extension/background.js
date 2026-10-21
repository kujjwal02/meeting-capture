// Service worker. It numbers call events, keeps each one in chrome.storage.local until the app
// acks it, and holds the native messaging port to the app. Chrome may stop this worker at any
// time, so every piece of state that matters lives in storage. session-rules.js decides what each
// tab message, tab change and alarm does; this file reads and writes storage and runs the effects.
//
// Storage keys: one per field of the session state in session-rules.js, plus
//   q:<seq>       an event message waiting for its ack
//   settings      the app's [meet] settings, which main.js reads
//   reloaded_for  the folder version mc-meet last reloaded itself for

import { decide, hasOpenSessions, restoreState, STATE_KEYS } from "./session-rules.js";

const HOST_NAME = "com.meetingcapture.app";
const PROTOCOL = 1;
const BACKOFF_MIN_MS = 1000;
const BACKOFF_MAX_MS = 60000;
const RECONNECT_ALARM = "mc-meet-reconnect";
const PENDING_END_ALARM = "mc-meet-pending-end";
const QUEUE_PREFIX = "q:";
const SETTINGS_KEY = "settings";
const RELOADED_FOR_KEY = "reloaded_for";

let nativePort = null;
let isWelcomed = false;
let backoffMs = BACKOFF_MIN_MS;
let reconnectTimer = null;
// The folder version to reload for once no call is open, or null.
let pendingReloadFor = null;

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
  if (port.name !== "mc-meet-tab") return;
  const tabId = port.sender?.tab?.id;
  port.onMessage.addListener((message) => {
    if (tabId === undefined) return;
    serially(() => apply({ kind: "tab_message", tabId, message }, port));
  });
  port.onDisconnect.addListener(() => {
    void chrome.runtime.lastError;
    if (tabId !== undefined) serially(() => apply({ kind: "tab_gone", tabId }));
  });
});

chrome.tabs.onRemoved.addListener((tabId) => {
  serially(() => apply({ kind: "tab_closed", tabId }));
});

// ---- Native port to the app.

async function hasWork() {
  if ((await store.unacked()).length > 0) return true;
  return hasOpenSessions(restoreState(await store.readState(), Date.now()));
}

async function resendUnacked() {
  for (const message of await store.unacked()) nativePort?.postMessage(message);
}

async function onAck(seq) {
  const acked = await store.removeAcked(seq);
  // After the last call ends and nothing is left to send, close the port so the host can exit.
  // Only a call_ended ack can make the worker idle, so only that ack pays for the full check.
  if (acked?.event?.kind !== "call_ended") return;
  if (nativePort && !(await hasWork())) {
    nativePort.disconnect();
    nativePort = null;
    isWelcomed = false;
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

function connectNative() {
  let port;
  try {
    port = chrome.runtime.connectNative(HOST_NAME);
  } catch (error) {
    console.warn("mc-meet: connectNative failed:", error);
    scheduleReconnect();
    return;
  }
  nativePort = port;
  isWelcomed = false;
  port.onMessage.addListener((message) => {
    if (message?.type === "welcome") {
      isWelcomed = true;
      backoffMs = BACKOFF_MIN_MS;
      console.info("mc-meet: app", message.app_version, "open sessions", message.open_sessions);
      storeSettings(message.settings);
      serially(() => noteFolderVersion(message.folder_version));
      serially(resendUnacked);
    } else if (message?.type === "settings") {
      storeSettings(message.settings);
    } else if (message?.type === "ack") {
      serially(() => onAck(message.seq));
    } else if (message?.type === "app_unavailable") {
      console.warn("mc-meet: the app did not start");
      port.disconnect();
      dropNative(port);
    }
  });
  port.onDisconnect.addListener(() => {
    console.warn("mc-meet: native port closed:", chrome.runtime.lastError?.message ?? "no error");
    dropNative(port);
  });
  port.postMessage({
    type: "hello",
    protocol: PROTOCOL,
    extension_version: chrome.runtime.getManifest().version,
  });
}

// An older app sends no settings; the stored ones stay.
function storeSettings(settings) {
  if (!settings) return;
  chrome.storage.local.set({ [SETTINGS_KEY]: settings }).catch((error) => {
    console.error("mc-meet: could not store the settings:", error);
  });
}

function ensureNative() {
  if (nativePort || reconnectTimer !== null) return;
  serially(async () => {
    if (!nativePort && (await hasWork())) connectNative();
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
ensureNative();
