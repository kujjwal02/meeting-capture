// Runs one real Meet call through mc-meet and a dev app, then checks the call log and the note.
// Two patchrome browsers join the call: the host, with mc-meet loaded and a fake microphone that
// speaks, and a guest on a second Google account, who sends a chat message.
// Usage: node tools/meet-e2e/run.mjs [path to the meeting-capture binary]
import { execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const appBinary = process.argv[2] ?? join(repoDir, "target", "debug", "meeting-capture");
// Outside ~/Documents: macOS privacy controls stop Chrome from starting a host there.
const workDir = process.env.MEET_E2E_DIR ?? join(homedir(), ".cache", "meeting-capture-e2e");
const hostProfile = process.env.MEET_E2E_HOST_PROFILE ?? "mcdev";
const guestProfile = process.env.MEET_E2E_GUEST_PROFILE ?? "default";
// Its own name and bundle id, so macOS lists the test build apart from the installed app.
const devAppName = "Meeting Capture Dev";
const devBundleId = "com.ujjwalkumar.meetingcapture.dev";
const signingIdentity = process.env.MEET_E2E_SIGNING_IDENTITY ?? devAppName;
const homeDir = join(workDir, "home");
const vaultDir = join(workDir, "vault");
const logDir = join(homeDir, "data", "browser-calls");
// The app registers its native host with each of these browsers at start. patchrome runs Chrome.
const hostManifestPaths = [
  "Google/Chrome",
  "Chromium",
  "BraveSoftware/Brave-Browser",
  "Microsoft Edge",
].map((browser) =>
  join(
    homedir(),
    "Library/Application Support",
    browser,
    "NativeMessagingHosts/com.meetingcapture.app.json",
  ),
);
const hostManifestPath = hostManifestPaths[0];
const runId = new Date().toISOString().replace(/[-:.]/g, "").slice(0, 15);
const chatText = `meet-e2e chat ${runId}`;
const startedMs = Date.now();

const istClock = new Intl.DateTimeFormat("en-GB", {
  timeZone: "Asia/Kolkata",
  timeStyle: "medium",
});
const step = (text) => console.log(`${istClock.format(new Date())} IST ${text}`);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function until(what, timeoutMs, check) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const found = await check();
    if (found) return found;
    if (Date.now() > deadline) throw new Error(`timed out after ${timeoutMs / 1000} s: ${what}`);
    await sleep(1500);
  }
}

// ---- patchrome, one session per browser.

const browsers = {
  host: { profile: hostProfile, session: `meet-e2e-host-${runId}` },
  guest: { profile: guestProfile, session: `meet-e2e-guest-${runId}` },
};

function patchrome(who, ...args) {
  const { profile, session } = browsers[who];
  const profileArgs = profile === "default" ? [] : ["--profile", profile];
  return execFileSync(
    "npx",
    ["-y", "patchrome@latest", ...profileArgs, "--session", session, ...args],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
}

// The named nodes of one role on the page, from a fresh accessibility snapshot.
function nodes(who, role) {
  const snapshotPath = join(workDir, `${who}-snapshot.txt`);
  patchrome(who, "snapshot", "--out", snapshotPath);
  const line = new RegExp(`${role} "([^"]*)"[^\\n]*?\\[ref=(\\w+)\\]`, "g");
  return [...readFileSync(snapshotPath, "utf8").matchAll(line)].map(([, name, ref]) => ({
    name,
    ref,
  }));
}

const findButton = (who, pattern) => nodes(who, "button").find((node) => pattern.test(node.name));

async function press(who, pattern, timeoutMs = 30000) {
  const button = await until(`${who} sees a button ${pattern}`, timeoutMs, () =>
    findButton(who, pattern),
  );
  patchrome(who, "click", `@${button.ref}`);
  return button.name;
}

function pressIfShown(who, pattern) {
  const button = findButton(who, pattern);
  if (button) patchrome(who, "click", `@${button.ref}`);
}

// ---- Chrome DevTools protocol on the host browser: loading mc-meet and the fake microphone.

async function devtools() {
  const status = execFileSync(
    "npx",
    ["-y", "patchrome@latest", "--profile", hostProfile, "daemon", "status"],
    {
      encoding: "utf8",
    },
  );
  const httpUrl = status.match(/devtools: (\S+)/)?.[1];
  if (!httpUrl)
    throw new Error(
      `the ${hostProfile} profile has no devtools address; it must be a debug profile`,
    );
  const { webSocketDebuggerUrl } = await (await fetch(`${httpUrl}/json/version`)).json();
  const socket = new WebSocket(webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.onopen = resolve;
    socket.onerror = reject;
  });
  let nextId = 0;
  const waiting = new Map();
  // Console lines and exceptions from the Meet tab and mc-meet's worker, printed on a failure.
  const pageLog = [];
  socket.onmessage = (message) => {
    const reply = JSON.parse(message.data);
    if (reply.id !== undefined) return waiting.get(reply.id)?.(reply);
    if (reply.method === "Runtime.exceptionThrown") {
      const details = reply.params.exceptionDetails;
      pageLog.push(`exception ${details.exception?.description ?? details.text}`);
    } else if (
      reply.method === "Runtime.consoleAPICalled" &&
      /warn|error/.test(reply.params.type)
    ) {
      const text = reply.params.args.map((arg) => arg.value ?? arg.description ?? "").join(" ");
      pageLog.push(`${reply.params.type} ${text}`);
    }
  };
  const send = (method, params = {}, sessionId = undefined) => {
    const id = ++nextId;
    socket.send(JSON.stringify({ id, method, params, sessionId }));
    return new Promise((resolve) => waiting.set(id, resolve));
  };
  return { send, pageLog, profileDir: profileDirOf(httpUrl), close: () => socket.close() };
}

// Chrome derives an unpacked extension's ID from its manifest key.
function extensionId(extensionDir) {
  const { key } = JSON.parse(readFileSync(join(extensionDir, "manifest.json"), "utf8"));
  const hex = createHash("sha256").update(Buffer.from(key, "base64")).digest("hex").slice(0, 32);
  return [...hex].map((digit) => String.fromCharCode(97 + Number.parseInt(digit, 16))).join("");
}

function fakeMicScript() {
  const aiff = join(workDir, "speech.aiff");
  const wav = join(workDir, "speech.wav");
  if (!existsSync(wav)) {
    const words =
      "This is the meeting capture end to end test. The host talks about the quarterly plan. " +
      "We ship the extension this week, then we review the notes on Friday. " +
      "Please check that every caption line lands in the call log with a speaker name.";
    execFileSync("say", ["-o", aiff, words]);
    execFileSync("afconvert", ["-f", "WAVE", "-d", "LEI16@16000", aiff, wav]);
  }
  const template = readFileSync(join(repoDir, "tools", "meet-e2e", "fake-mic.js"), "utf8");
  return template.replace("__SPEECH_BASE64__", readFileSync(wav).toString("base64"));
}

// ---- The dev app, with its own home, started as the native host's target.

function startApp() {
  mkdirSync(join(homeDir, "config"), { recursive: true });
  mkdirSync(vaultDir, { recursive: true });
  const settingsPath = join(homeDir, "config", "settings.toml");
  if (!existsSync(settingsPath))
    writeFileSync(settingsPath, `vault_dir = ${JSON.stringify(vaultDir)}\n`);
  const bundle = join(workDir, `${devAppName}.app`);
  const binary = join(bundle, "Contents", "MacOS", "meeting-capture");
  mkdirSync(dirname(binary), { recursive: true });
  writeFileSync(join(bundle, "Contents", "Info.plist"), devInfoPlist());
  copyFileSync(appBinary, binary);
  // The linker's ad-hoc signature changes with each build, so macOS asks for Accessibility again
  // on every run. A stable identity keeps the grant across builds.
  try {
    execFileSync("codesign", ["--force", "--sign", signingIdentity, bundle], {
      stdio: ["ignore", "ignore", "pipe"],
    });
  } catch (error) {
    step(
      `could not sign the dev app with "${signingIdentity}", so macOS may ask for Accessibility: ${String(error.stderr).trim()}`,
    );
  }
  const wrapper = join(workDir, "host.sh");
  writeFileSync(
    wrapper,
    `#!/bin/sh\nMEETING_CAPTURE_HOME=${JSON.stringify(homeDir)} exec ${JSON.stringify(binary)} "$@" 2>> ${JSON.stringify(join(workDir, "host.log"))}\n`,
    { mode: 0o755 },
  );
  const app = spawn(binary, [], {
    env: { ...process.env, MEETING_CAPTURE_HOME: homeDir },
    stdio: ["ignore", "ignore", "ignore"],
    detached: true,
  });
  return { app, binary, wrapper };
}

function devInfoPlist() {
  const entries = {
    CFBundleExecutable: "meeting-capture",
    CFBundleIdentifier: devBundleId,
    CFBundleName: devAppName,
    CFBundleDisplayName: devAppName,
    CFBundlePackageType: "APPL",
    NSMicrophoneUsageDescription: "Meeting Capture records your side of a call.",
    NSAudioCaptureUsageDescription: "Meeting Capture records the other side of a call.",
  };
  const keys = Object.entries(entries)
    .map(([key, value]) => `  <key>${key}</key>\n  <string>${value}</string>\n`)
    .join("");
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
${keys}  <key>LSUIElement</key>
  <true/>
</dict>
</plist>
`;
}

// The Chrome data folder of the browser behind a devtools address. patchrome starts Chrome on
// port 0, and Chrome writes the port it chose into DevToolsActivePort in that folder.
function profileDirOf(httpUrl) {
  const port = new URL(httpUrl).port;
  const commands = execFileSync("ps", ["-axww", "-o", "command="], { encoding: "utf8" });
  const dirs = commands
    .split("\n")
    .map((command) => command.match(/--user-data-dir=(.+?)(?= --|$)/)?.[1])
    .filter(Boolean);
  const dir = dirs.find((candidate) => {
    const portFile = join(candidate, "DevToolsActivePort");
    return existsSync(portFile) && readFileSync(portFile, "utf8").split("\n")[0] === port;
  });
  if (!dir) throw new Error(`found no Chrome data folder for devtools port ${port}`);
  return dir;
}

// The app writes the global manifests at start with its own path. Host mode then needs the home
// override, which only the wrapper sets. A browser started with its own data folder reads the
// manifest in that folder first, so the wrapper goes there.
function pointHostAt(wrapper, appBinaryPath, profileManifestPath) {
  const manifest = JSON.parse(readFileSync(hostManifestPath, "utf8"));
  if (manifest.path !== appBinaryPath) return false;
  mkdirSync(dirname(profileManifestPath), { recursive: true });
  writeFileSync(
    profileManifestPath,
    `${JSON.stringify({ ...manifest, path: wrapper }, null, 2)}\n`,
  );
  return true;
}

// ---- What the app wrote.

function readLog(meetingCode) {
  if (!existsSync(logDir)) return null;
  for (const name of readdirSync(logDir).filter((file) => file.endsWith(".jsonl"))) {
    const path = join(logDir, name);
    if (statSync(path).mtimeMs < startedMs) continue;
    const events = readFileSync(path, "utf8")
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line).event);
    if (
      events.some((event) => event.kind === "call_started" && event.meeting_code === meetingCode)
    ) {
      return { path, events };
    }
  }
  return null;
}

function findNote(meetingCode) {
  const notes = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) walk(path);
      else if (entry.name.endsWith(".md") && statSync(path).mtimeMs >= startedMs) notes.push(path);
    }
  };
  walk(vaultDir);
  return notes.find((path) => readFileSync(path, "utf8").includes(meetingCode));
}

// ---- The run.

const manifestsBefore = new Map(
  hostManifestPaths
    .filter((path) => existsSync(path))
    .map((path) => [path, readFileSync(path, "utf8")]),
);
const cdp = await devtools();
const { app, binary, wrapper } = startApp();
const profileManifestPath = join(
  cdp.profileDir,
  "NativeMessagingHosts/com.meetingcapture.app.json",
);
const profileManifestBefore = existsSync(profileManifestPath)
  ? readFileSync(profileManifestPath, "utf8")
  : undefined;
const failures = [];
let workerSessionId;
const expect = (isTrue, text) => {
  step(`${isTrue ? "ok  " : "FAIL"} ${text}`);
  if (!isTrue) failures.push(text);
};

try {
  step(`dev app ${app.pid} with home ${homeDir}`);
  await until("the dev app writes the host manifest", 30000, () =>
    pointHostAt(wrapper, binary, profileManifestPath),
  );

  const extensionDir = join(repoDir, "extension");
  await cdp.send("Extensions.uninstall", { id: extensionId(extensionDir) });
  const loaded = await cdp.send("Extensions.loadUnpacked", { path: extensionDir });
  if (loaded.error) throw new Error(`could not load mc-meet: ${loaded.error.message}`);
  step(`mc-meet ${loaded.result.id} loaded in the host browser`);
  const worker = await until("mc-meet's worker starts", 10000, async () => {
    const { result } = await cdp.send("Target.getTargets");
    return result.targetInfos.find(
      (info) =>
        info.type === "service_worker" &&
        info.url.startsWith(`chrome-extension://${loaded.result.id}/`),
    );
  });
  const { result: workerSession } = await cdp.send("Target.attachToTarget", {
    targetId: worker.targetId,
    flatten: true,
  });
  await cdp.send("Runtime.enable", {}, workerSession.sessionId);
  workerSessionId = workerSession.sessionId;

  patchrome("host", "session", "label", "meet-e2e host");
  patchrome("host", "open", "https://meet.google.com/landing");
  const { result: targets } = await cdp.send("Target.getTargets");
  const landing = targets.targetInfos.filter(
    (info) => info.type === "page" && info.url.includes("meet.google.com"),
  );
  const target = landing.at(-1);
  const { result: attached } = await cdp.send("Target.attachToTarget", {
    targetId: target.targetId,
    flatten: true,
  });
  const tab = attached.sessionId;
  await cdp.send("Page.enable", {}, tab);
  await cdp.send("Runtime.enable", {}, tab);
  await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: fakeMicScript() }, tab);
  await cdp.send("Page.navigate", { url: "https://meet.new" }, tab);
  const meetingUrl = await until("the host gets a meeting code", 30000, async () => {
    const { result } = await cdp.send(
      "Runtime.evaluate",
      { expression: "location.href", returnByValue: true },
      tab,
    );
    const url = result?.result?.value ?? "";
    return /meet\.google\.com\/[a-z]{3}-[a-z]{4}-[a-z]{3}/.test(url) ? url.split("?")[0] : null;
  });
  const meetingCode = meetingUrl.split("/").at(-1);
  step(`meeting ${meetingCode}`);

  if (findButton("host", /^Switch here$/))
    throw new Error("the host account is in a call on another device");
  // meet.new often joins straight away; otherwise it shows the join screen.
  await until("the host sees the join screen or the call", 30000, () =>
    findButton("host", /^(Join now|Leave call)$/),
  );
  pressIfShown("host", /^Join now$/);
  await until("the host is in the call", 30000, () => findButton("host", /^Leave call$/));
  // A fake microphone's AudioContext starts suspended until a user gesture.
  await cdp.send(
    "Runtime.evaluate",
    { expression: "window.__fakeMicContext?.resume()", awaitPromise: true, userGesture: true },
    tab,
  );

  patchrome("guest", "session", "label", "meet-e2e guest");
  patchrome("guest", "open", meetingUrl);
  await until("the guest sees the join screen", 60000, () =>
    findButton("guest", /^(Ask to join|Join now)$/),
  );
  pressIfShown("guest", /^Not now$/);
  pressIfShown("guest", /^Turn off microphone$/);
  pressIfShown("guest", /^Turn off camera$/);
  await press("guest", /^(Ask to join|Join now)$/);
  // Meet shows Admit in a toast or in the People panel, depending on the layout. "Admit one
  // guest" only opens the panel.
  let isPeopleOpen = false;
  const admit = await until("the host sees the guest waiting", 45000, () => {
    const button = findButton("host", /^Admit(?! (one guest|\d+ guests)$)\b/);
    if (button) return button;
    pressIfShown("host", /^Admit (one guest|\d+ guests)$/);
    if (!isPeopleOpen) pressIfShown("host", /^People$/);
    isPeopleOpen = true;
  });
  patchrome("host", "click", `@${admit.ref}`);
  // The lobby has a Leave call button too. Only the call has the captions button.
  await until("the guest is in the call", 30000, () =>
    findButton("guest", /^Turn (on|off) captions$/),
  );
  step("both browsers are in the call");

  const withCaptions = await until("3 captions in the call log", 90000, () => {
    const log = readLog(meetingCode);
    return log && log.events.filter((event) => event.kind === "caption").length >= 3 ? log : null;
  });
  step(`call log ${withCaptions.path}`);

  await press("guest", /^Chat with everyone$/);
  const sentMs = Date.now();
  const chatBox = await until("the guest sees the chat box", 30000, () => {
    const box = nodes("guest", "textbox").find((node) => /message/i.test(node.name));
    if (!box)
      step(
        `  guest textboxes: ${JSON.stringify(nodes("guest", "textbox").map((node) => node.name))}`,
      );
    return box;
  });
  patchrome("guest", "fill", `@${chatBox.ref}`, chatText);
  patchrome("guest", "press", "Enter");
  const chat = await until("the guest's chat in the call log", 30000, () =>
    readLog(meetingCode)?.events.find((event) => event.kind === "chat" && event.text === chatText),
  );

  await sleep(10000);
  await press("guest", /^Leave call$/);
  await press("host", /^Leave call$/);
  const ended = await until("call_ended in the call log", 60000, () => {
    const log = readLog(meetingCode);
    return log?.events.at(-1)?.kind === "call_ended" ? log : null;
  });
  const notePath = await until("the note in the vault", 180000, () => findNote(meetingCode));

  const { events } = ended;
  const captions = events.filter((event) => event.kind === "caption");
  const rosters = events.filter((event) => event.kind === "roster");
  const healths = events.filter((event) => event.kind === "health");
  const note = readFileSync(notePath, "utf8");
  expect(events[0].kind === "call_started", "the log starts with call_started");
  expect(
    events.filter((event) => event.kind === "call_started").length === 1,
    "one session in the log",
  );
  expect(captions.length >= 3, `${captions.length} captions`);
  expect(
    captions.every((caption) => caption.source === "channel"),
    "every caption came from the data channel",
  );
  expect(
    Math.max(...rosters.map((roster) => roster.participants.length)) >= 2,
    "the roster lists both people",
  );
  expect(
    Math.abs(Date.parse(chat.at) - sentMs) < 15000,
    `chat time within 15 s of sending (${chat.at})`,
  );
  expect(healths.at(-1)?.captions_on === true, "the last health event has captions on");
  expect(note.includes(chatText), "the note has the chat message");
  step(`note ${notePath}`);
} catch (error) {
  failures.push(error.message);
  step(`FAIL ${error.message}`);
  for (const line of cdp.pageLog.slice(-30)) step(`  page: ${line.slice(0, 400)}`);
  if (workerSessionId) {
    const stored = await cdp.send(
      "Runtime.evaluate",
      {
        expression: "chrome.storage.local.get(null).then((state) => JSON.stringify(state))",
        awaitPromise: true,
        returnByValue: true,
      },
      workerSessionId,
    );
    step(`  worker state: ${String(stored.result?.result?.value).slice(0, 800)}`);
  }
  for (const who of ["host", "guest"]) {
    try {
      patchrome(who, "screenshot", "--out", join(workDir, `${who}-failure.png`));
    } catch {}
  }
} finally {
  for (const who of ["host", "guest"]) {
    try {
      patchrome(who, "close");
    } catch {}
  }
  cdp.close();
  process.kill(app.pid);
  for (const [path, manifest] of manifestsBefore) writeFileSync(path, manifest);
  if (profileManifestBefore === undefined) rmSync(profileManifestPath, { force: true });
  else writeFileSync(profileManifestPath, profileManifestBefore);
  step("dev app stopped, host manifests restored");
}

if (failures.length > 0) {
  console.error(`${failures.length} failed: ${failures.join("; ")}`);
  process.exit(1);
}
step("passed");
