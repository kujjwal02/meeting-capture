// Service worker. It numbers call events, keeps each one in chrome.storage.local until the app
// acks it, and holds the native messaging port to the app. Chrome may stop this worker at any
// time, so every piece of state that matters lives in storage.
//
// Storage keys:
//   nextSeq       the next event seq for this install
//   q:<seq>       an event message waiting for its ack
//   tabSessions   { [tabId]: session_id } for tabs in a call
//   lastN         { [session_id]: n } the last content script event number stored. One small
//                 entry per call; never pruned.
//   pendingEnds   { [session_id]: { tab_id, at_ms } } sessions whose page went away
//   endedSessions the last session IDs that got a call_ended, newest last

const HOST_NAME = "com.meetingcapture.app";
const PROTOCOL = 1;
// A page that unloads mid-call has this long to come back to the same session.
const RESUME_WINDOW_MS = 5 * 60 * 1000;
const BACKOFF_MIN_MS = 1000;
const BACKOFF_MAX_MS = 60000;
const RECONNECT_ALARM = "mc-meet-reconnect";
const PENDING_END_ALARM = "mc-meet-pending-end";
const QUEUE_PREFIX = "q:";
const ENDED_SESSIONS_KEPT = 50;

let nativePort = null;
let isWelcomed = false;
let backoffMs = BACKOFF_MIN_MS;
let reconnectTimer = null;
let nextSeq = null;

// Every storage change and native send runs on this chain, so seq order holds on the wire.
let chain = Promise.resolve();
function serially(task) {
  const run = chain.then(task);
  chain = run.catch((error) => console.error("mc-meet:", error));
  return run;
}

async function readState() {
  const stored = await chrome.storage.local.get([
    "tabSessions",
    "lastN",
    "pendingEnds",
    "endedSessions",
  ]);
  return {
    tabSessions: stored.tabSessions ?? {},
    lastN: stored.lastN ?? {},
    pendingEnds: stored.pendingEnds ?? {},
    endedSessions: stored.endedSessions ?? [],
  };
}

function markEnded(state, sessionId) {
  state.endedSessions = [...state.endedSessions.filter((s) => s !== sessionId), sessionId].slice(
    -ENDED_SESSIONS_KEPT,
  );
}

async function takeSeq() {
  if (nextSeq === null) nextSeq = (await chrome.storage.local.get("nextSeq")).nextSeq ?? 1;
  const seq = nextSeq;
  nextSeq += 1;
  return seq;
}

// Stores one event with its seq, plus any state changes, in a single write. Runs on the chain.
async function storeEvent(sessionId, event, extraState = {}) {
  const seq = await takeSeq();
  const message = { type: "event", seq, session_id: sessionId, event };
  await chrome.storage.local.set({ nextSeq, [`${QUEUE_PREFIX}${seq}`]: message, ...extraState });
  if (nativePort && isWelcomed) nativePort.postMessage(message);
  else ensureNative();
  return seq;
}

function setDiscardable(tabId, isDiscardable) {
  chrome.tabs.update(Number(tabId), { autoDiscardable: isDiscardable }).catch(() => {});
}

function endEvent(reason, atMs) {
  return { kind: "call_ended", at: new Date(atMs).toISOString(), reason };
}

// Ends a session the worker tracks. Runs on the chain.
async function endSession(state, sessionId, reason, atMs) {
  for (const [tabId, tabSession] of Object.entries(state.tabSessions)) {
    if (tabSession !== sessionId) continue;
    delete state.tabSessions[tabId];
    setDiscardable(tabId, true);
  }
  delete state.pendingEnds[sessionId];
  markEnded(state, sessionId);
  await storeEvent(sessionId, endEvent(reason, atMs), state);
}

// A tab starts or resumes a session. Any other session of that tab is over.
async function attachTab(state, tabId, sessionId) {
  const previous = state.tabSessions[tabId];
  if (previous && previous !== sessionId) {
    const unloadedMs = state.pendingEnds[previous]?.at_ms ?? Date.now();
    await endSession(state, previous, "page_unload", unloadedMs);
  }
  state.tabSessions[tabId] = sessionId;
  delete state.pendingEnds[sessionId];
  setDiscardable(tabId, false);
}

// ---- Content script ports.

async function onTabMessage(port, message) {
  const tabId = port.sender?.tab?.id;
  if (tabId === undefined) return;
  if (message.type === "resume") {
    const state = await readState();
    // The worker already ended this session: the page came back too late.
    if (state.endedSessions.includes(message.session_id)) {
      tellSessionEnded(port, message.session_id);
      return;
    }
    await attachTab(state, tabId, message.session_id);
    await chrome.storage.local.set(state);
    await schedulePendingEnds(state);
    return;
  }
  if (message.type !== "event") return;
  const state = await readState();
  const sessionId = message.session_id;
  if ((state.lastN[sessionId] ?? 0) >= message.n) {
    reply(port, sessionId, message.n);
    return;
  }
  const event = message.event;
  if (state.endedSessions.includes(sessionId) && event.kind !== "call_ended") {
    reply(port, sessionId, message.n);
    tellSessionEnded(port, sessionId);
    return;
  }
  if (event.kind === "call_started") {
    event.tab_id = tabId;
    await attachTab(state, tabId, sessionId);
  }
  if (event.kind === "call_ended") {
    delete state.tabSessions[tabId];
    delete state.pendingEnds[sessionId];
    markEnded(state, sessionId);
    setDiscardable(tabId, true);
  }
  // Kept after the call ends too, so a late resend of call_ended is not stored twice.
  state.lastN[sessionId] = message.n;
  await storeEvent(sessionId, event, state);
  await schedulePendingEnds(state);
  reply(port, sessionId, message.n);
}

function reply(port, sessionId, n) {
  try {
    port.postMessage({ type: "stored", session_id: sessionId, n });
  } catch {}
}

function tellSessionEnded(port, sessionId) {
  try {
    port.postMessage({ type: "session_ended", session_id: sessionId });
  } catch {}
}

// The page of a tab in a call went away: a reload, a navigation or a crash. The call ends
// unless the same tab resumes the session within the window.
async function onTabGone(tabId) {
  const state = await readState();
  const sessionId = state.tabSessions[tabId];
  if (!sessionId || state.pendingEnds[sessionId]) return;
  state.pendingEnds[sessionId] = { tab_id: Number(tabId), at_ms: Date.now() };
  await chrome.storage.local.set({ pendingEnds: state.pendingEnds });
  await schedulePendingEnds(state);
}

async function schedulePendingEnds(state) {
  const dueTimes = Object.values(state.pendingEnds).map((p) => p.at_ms + RESUME_WINDOW_MS);
  if (dueTimes.length === 0) {
    await chrome.alarms.clear(PENDING_END_ALARM);
    return;
  }
  await chrome.alarms.create(PENDING_END_ALARM, {
    when: Math.max(Math.min(...dueTimes), Date.now() + 1000),
  });
}

async function endDuePendingSessions() {
  const state = await readState();
  const nowMs = Date.now();
  for (const [sessionId, pending] of Object.entries(state.pendingEnds)) {
    if (pending.at_ms + RESUME_WINDOW_MS <= nowMs) {
      await endSession(state, sessionId, "page_unload", pending.at_ms);
    }
  }
  await schedulePendingEnds(state);
}

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== "mc-meet-tab") return;
  const tabId = port.sender?.tab?.id;
  port.onMessage.addListener((message) => {
    serially(() => onTabMessage(port, message));
  });
  port.onDisconnect.addListener(() => {
    void chrome.runtime.lastError;
    if (tabId !== undefined) serially(() => onTabGone(tabId));
  });
});

chrome.tabs.onRemoved.addListener((tabId) => {
  serially(async () => {
    const state = await readState();
    const sessionId = state.tabSessions[tabId];
    if (sessionId) await endSession(state, sessionId, "tab_closed", Date.now());
    await schedulePendingEnds(state);
  });
});

// ---- Native port to the app.

async function hasWork() {
  const stored = await chrome.storage.local.get(null);
  if (Object.keys(stored).some((key) => key.startsWith(QUEUE_PREFIX))) return true;
  return (
    Object.keys(stored.tabSessions ?? {}).length > 0 ||
    Object.keys(stored.pendingEnds ?? {}).length > 0
  );
}

async function resendUnacked() {
  const stored = await chrome.storage.local.get(null);
  const messages = Object.entries(stored)
    .filter(([key]) => key.startsWith(QUEUE_PREFIX))
    .map(([, message]) => message)
    .sort((a, b) => a.seq - b.seq);
  for (const message of messages) nativePort?.postMessage(message);
}

async function onAck(seq) {
  const key = `${QUEUE_PREFIX}${seq}`;
  const acked = (await chrome.storage.local.get(key))[key];
  await chrome.storage.local.remove(key);
  // After the last call ends and nothing is left to send, close the port so the host can exit.
  // Only a call_ended ack can make the worker idle, so only that ack pays for the full check.
  if (acked?.event?.kind !== "call_ended") return;
  if (nativePort && !(await hasWork())) {
    nativePort.disconnect();
    nativePort = null;
    isWelcomed = false;
  }
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
      serially(resendUnacked);
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

function ensureNative() {
  if (nativePort || reconnectTimer !== null) return;
  serially(async () => {
    if (!nativePort && (await hasWork())) connectNative();
  });
}

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === PENDING_END_ALARM) serially(endDuePendingSessions);
  if (alarm.name === RECONNECT_ALARM) ensureNative();
});

// After a browser restart no old tab is left, but a restored tab may resume its session.
chrome.runtime.onStartup.addListener(() => {
  serially(async () => {
    const state = await readState();
    const nowMs = Date.now();
    for (const sessionId of Object.values(state.tabSessions)) {
      state.pendingEnds[sessionId] ??= { tab_id: null, at_ms: nowMs };
    }
    state.tabSessions = {};
    await chrome.storage.local.set(state);
    await schedulePendingEnds(state);
  });
});

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
ensureNative();
