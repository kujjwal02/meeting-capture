// Toolbar popup: the active tab's call status as the app last reported it, and a button that
// asks the app for a debug report. It keeps a port to the worker open, which keeps the native
// port to the app open while the popup shows.
(() => {
  const CALL_STATUS_KEY = "call_status";
  const APP_KEY = "app";
  const STATE_WORDS = {
    waiting: "Waiting for captions",
    recording: "Recording",
    problem: "Not recording",
  };
  const SOURCE_WORDS = { channel: "call", dom: "page", none: "none yet" };
  const ICONS = { waiting: "idle", recording: "recording", problem: "problem" };
  // The app answers once the report is written; a long call log takes a few seconds.
  const REPORT_WAIT_MS = 30000;

  const byId = (id) => document.getElementById(id);
  let tabId;
  let callStatus;
  let app = { is_reachable: true };
  let sessionId;
  let port;
  let reportTimer = null;

  function secondsAgo(at) {
    const seconds = Math.max(0, Math.round((Date.now() - Date.parse(at)) / 1000));
    if (seconds < 120) return `${seconds} s ago`;
    return `${Math.round(seconds / 60)} min ago`;
  }

  function show(id, text) {
    byId(id).textContent = text;
  }

  function render() {
    const call = sessionId
      ? callStatus?.calls?.find((candidate) => candidate.session_ids.includes(sessionId))
      : undefined;
    const isInCall = sessionId !== undefined;
    let icon = "idle";
    let line = isInCall ? STATE_WORDS.waiting : "No call in this tab";
    if (!app.is_reachable) {
      icon = "off";
      line = "Meeting Capture is not running";
    } else if (call) {
      icon = ICONS[call.state] ?? "idle";
      line = STATE_WORDS[call.state] ?? call.state;
      if (call.state === "recording" && call.recording_since_label) {
        line = `Recording since ${call.recording_since_label}`;
      }
    }
    byId("state-icon").src = `../icons/${icon}-32.png`;
    show("state-line", line);

    byId("reason").hidden = !call?.reason_text;
    show("reason", call?.reason_text ?? "");

    byId("call").hidden = !call;
    if (call) {
      show("title", call.title || "Untitled call");
      show("started", call.started_at_label);
      show("captions", String(call.caption_count));
      show("last-caption", call.last_caption_at ? secondsAgo(call.last_caption_at) : "none yet");
      show("source", call.caption_source ? SOURCE_WORDS[call.caption_source] : "none yet");
    }

    const problems = call?.problems ?? [];
    byId("problems").hidden = problems.length === 0;
    byId("problem-list").replaceChildren(
      ...problems
        .slice()
        .reverse()
        .map((problem) => {
          const entry = document.createElement("li");
          entry.textContent = `${problem.at_label}: ${problem.reason_text}`;
          return entry;
        }),
    );

    show(
      "app",
      app.is_reachable
        ? `connected${app.app_version ? `, ${app.app_version}` : ""}`
        : "not connected",
    );
    show("extension", chrome.runtime.getManifest().version);
  }

  async function load() {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    tabId = tab?.id;
    const local = await chrome.storage.local.get("tabSessions");
    sessionId = tabId === undefined ? undefined : local.tabSessions?.[tabId];
    const stored = await chrome.storage.session.get([CALL_STATUS_KEY, APP_KEY]);
    callStatus = stored[CALL_STATUS_KEY];
    app = stored[APP_KEY] ?? { is_reachable: true };
    render();
  }

  function showReportOutcome(text) {
    clearTimeout(reportTimer);
    reportTimer = null;
    const outcome = byId("report-outcome");
    outcome.hidden = false;
    outcome.textContent = text;
  }

  function saveReport(isWithText) {
    showReportOutcome("Saving");
    reportTimer = setTimeout(
      () => showReportOutcome("Not saved: Meeting Capture did not answer within 30 s."),
      REPORT_WAIT_MS,
    );
    port.postMessage({ type: "save_debug_report", tab_id: tabId, with_text: isWithText });
  }

  function onWorkerMessage(message) {
    if (message?.type === "debug_report_saved") {
      showReportOutcome(`Saved to ${message.path}`);
    } else if (message?.type === "debug_report_failed") {
      showReportOutcome(`Not saved: ${message.error}`);
    } else if (message?.type === "app_changed") {
      load();
    }
  }

  port = chrome.runtime.connect({ name: "mc-meet-popup" });
  port.onMessage.addListener(onWorkerMessage);
  byId("save-report").addEventListener("click", () => saveReport(false));
  byId("save-report-with-text").addEventListener("click", () => saveReport(true));
  // Queued events change local storage many times a second during a call.
  chrome.storage.onChanged.addListener((changes, area) => {
    const isStatusChange =
      (area === "session" && (CALL_STATUS_KEY in changes || APP_KEY in changes)) ||
      (area === "local" && "tabSessions" in changes);
    if (isStatusChange) load();
  });
  setInterval(render, 1000);
  load();
})();
