// The service worker's session rules and event numbering, with no chrome.* calls and no clock.
// background.js reads the state, calls decide, writes the new state and the queued event
// messages in one storage write, and then runs the effects. The module also loads in plain Node.
//
// State, one field per storage key:
//   nextSeq       the next event seq for this install
//   tabSessions   { [tabId]: session_id } for tabs in a call
//   lastN         { [session_id]: n } the last content script event number stored. One small
//                 entry per call; never pruned.
//   pendingEnds   { [session_id]: { tab_id, at_ms } } sessions whose page went away
//   endedSessions the last session IDs that got a call_ended, newest last
//
// Inputs, by kind:
//   tab_message   { tabId, message } a resume or event message from a tab's content script
//   tab_gone      { tabId } the content script port of a tab closed
//   tab_closed    { tabId } the tab itself closed
//   pending_due   the pending end alarm fired
//   tabs_missing  { tabIds } tracked tabs that no longer exist when the worker starts
//
// Effects, by kind:
//   stored             { sessionId, n } tell the tab its event is stored
//   session_ended      { sessionId } tell the tab the worker already ended its session
//   set_discardable    { tabId, isDiscardable }
//   set_pending_alarm  { whenMs } the earliest time a pending end falls due
//   clear_pending_alarm

import "./call-events.js";

// A page that unloads mid-call has this long to come back to the same session.
export const RESUME_WINDOW_MS = 5 * 60 * 1000;
export const STATE_KEYS = ["nextSeq", "tabSessions", "lastN", "pendingEnds", "endedSessions"];
const ENDED_SESSIONS_KEPT = 50;

// A fresh install starts seq at the install time in microseconds. A reinstall mid-call resumes the
// tab's session, and the app drops any seq that session's log already has.
export function restoreState(stored, nowMs) {
  return {
    nextSeq: stored.nextSeq ?? nowMs * 1000,
    tabSessions: stored.tabSessions ?? {},
    lastN: stored.lastN ?? {},
    pendingEnds: stored.pendingEnds ?? {},
    endedSessions: stored.endedSessions ?? [],
  };
}

export function hasOpenSessions(state) {
  return Object.keys(state.tabSessions).length > 0 || Object.keys(state.pendingEnds).length > 0;
}

// Returns { state, queued, effects }. state is the input object itself when nothing changed, so
// the caller skips the write. queued holds event messages in seq order, to store with the state.
export function decide(state, input, nowMs) {
  const outcome = { state: structuredClone(state), queued: [], effects: [], isChanged: false };
  switch (input.kind) {
    case "tab_message":
      onTabMessage(outcome, input.tabId, input.message, nowMs);
      break;
    case "tab_gone":
      onTabGone(outcome, input.tabId, nowMs);
      break;
    case "tab_closed": {
      const sessionId = outcome.state.tabSessions[input.tabId];
      if (sessionId) endSession(outcome, sessionId, "tab_closed", nowMs);
      schedulePendingEnds(outcome, nowMs);
      break;
    }
    case "pending_due":
      endDuePendingSessions(outcome, nowMs);
      break;
    case "tabs_missing":
      holdSessionsOfMissingTabs(outcome, input.tabIds, nowMs);
      break;
  }
  return {
    state: outcome.isChanged ? outcome.state : state,
    queued: outcome.queued,
    effects: outcome.effects,
  };
}

function onTabMessage(outcome, tabId, message, nowMs) {
  const state = outcome.state;
  if (message.type === "resume") {
    // The worker already ended this session: the page came back too late.
    if (state.endedSessions.includes(message.session_id)) {
      outcome.effects.push({ kind: "session_ended", sessionId: message.session_id });
      return;
    }
    attachTab(outcome, tabId, message.session_id, nowMs);
    schedulePendingEnds(outcome, nowMs);
    return;
  }
  if (message.type !== "event") return;
  const sessionId = message.session_id;
  const stored = { kind: "stored", sessionId, n: message.n };
  if ((state.lastN[sessionId] ?? 0) >= message.n) {
    outcome.effects.push(stored);
    return;
  }
  let event = message.event;
  if (state.endedSessions.includes(sessionId) && event.kind !== "call_ended") {
    outcome.effects.push(stored, { kind: "session_ended", sessionId });
    return;
  }
  if (event.kind === "call_started") {
    event = { ...event, tab_id: tabId };
    attachTab(outcome, tabId, sessionId, nowMs);
  }
  if (event.kind === "call_ended") {
    delete state.tabSessions[tabId];
    delete state.pendingEnds[sessionId];
    markEnded(state, sessionId);
    outcome.effects.push({ kind: "set_discardable", tabId, isDiscardable: true });
  }
  // Kept after the call ends too, so a late resend of call_ended is not stored twice.
  state.lastN[sessionId] = message.n;
  queueEvent(outcome, sessionId, event);
  schedulePendingEnds(outcome, nowMs);
  outcome.effects.push(stored);
}

// The page of a tab in a call went away: a reload, a navigation or a crash. The call ends
// unless the same tab resumes the session within the window.
function onTabGone(outcome, tabId, nowMs) {
  const state = outcome.state;
  const sessionId = state.tabSessions[tabId];
  if (!sessionId || state.pendingEnds[sessionId]) return;
  state.pendingEnds[sessionId] = { tab_id: Number(tabId), at_ms: nowMs };
  outcome.isChanged = true;
  schedulePendingEnds(outcome, nowMs);
}

function endDuePendingSessions(outcome, nowMs) {
  for (const [sessionId, pending] of Object.entries(outcome.state.pendingEnds)) {
    if (pending.at_ms + RESUME_WINDOW_MS <= nowMs) {
      endSession(outcome, sessionId, "page_unload", pending.at_ms);
    }
  }
  schedulePendingEnds(outcome, nowMs);
}

// A session whose tab no longer exists lost its page with no tab close seen: a browser restart
// or crash, or the extension was off. A restored tab may still resume it.
function holdSessionsOfMissingTabs(outcome, tabIds, nowMs) {
  const state = outcome.state;
  for (const tabId of tabIds) {
    const sessionId = state.tabSessions[tabId];
    if (!sessionId) continue;
    state.pendingEnds[sessionId] ??= { tab_id: null, at_ms: nowMs };
    delete state.tabSessions[tabId];
    outcome.isChanged = true;
  }
  if (outcome.isChanged) schedulePendingEnds(outcome, nowMs);
}

// A tab starts or resumes a session. Any other session of that tab is over.
function attachTab(outcome, tabId, sessionId, nowMs) {
  const state = outcome.state;
  const previous = state.tabSessions[tabId];
  if (previous && previous !== sessionId) {
    const unloadedMs = state.pendingEnds[previous]?.at_ms ?? nowMs;
    endSession(outcome, previous, "page_unload", unloadedMs);
  }
  state.tabSessions[tabId] = sessionId;
  delete state.pendingEnds[sessionId];
  outcome.isChanged = true;
  outcome.effects.push({ kind: "set_discardable", tabId, isDiscardable: false });
}

// Ends a session the worker tracks, with a call_ended of its own.
function endSession(outcome, sessionId, reason, atMs) {
  const state = outcome.state;
  for (const [tabId, tabSession] of Object.entries(state.tabSessions)) {
    if (tabSession !== sessionId) continue;
    delete state.tabSessions[tabId];
    outcome.effects.push({ kind: "set_discardable", tabId, isDiscardable: true });
  }
  delete state.pendingEnds[sessionId];
  markEnded(state, sessionId);
  queueEvent(outcome, sessionId, globalThis.mcMeetCallEvents.callEnded({ atMs, reason }));
}

function markEnded(state, sessionId) {
  state.endedSessions = [...state.endedSessions.filter((s) => s !== sessionId), sessionId].slice(
    -ENDED_SESSIONS_KEPT,
  );
}

function queueEvent(outcome, sessionId, event) {
  const seq = outcome.state.nextSeq;
  outcome.state.nextSeq += 1;
  outcome.queued.push({ type: "event", seq, session_id: sessionId, event });
  outcome.isChanged = true;
}

function schedulePendingEnds(outcome, nowMs) {
  const dueTimes = Object.values(outcome.state.pendingEnds).map((p) => p.at_ms + RESUME_WINDOW_MS);
  if (dueTimes.length === 0) {
    outcome.effects.push({ kind: "clear_pending_alarm" });
    return;
  }
  outcome.effects.push({
    kind: "set_pending_alarm",
    whenMs: Math.max(Math.min(...dueTimes), nowMs + 1000),
  });
}
