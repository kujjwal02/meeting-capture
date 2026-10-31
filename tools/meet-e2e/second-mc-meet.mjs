// A second mc-meet as the app sees one, for run.mjs: a welcomed port that holds no session, as
// a worker in another Chrome profile would be. It runs as its own process because run.mjs
// blocks its event loop on patchrome calls, which would hold the answers back by seconds.
//
// On the first press notice it answers shortcut_press_noted at once, as that worker does, and
// 300 ms later acts out the press landing in its profile: it reports shortcut_press_landed.
// Each notice it hears goes to stdout as one JSON line.
//
// Usage: node second-mc-meet.mjs <host wrapper> <extension id>
import { spawn } from "node:child_process";

const [wrapper, extensionId] = process.argv.slice(2);
const host = spawn(wrapper, [`chrome-extension://${extensionId}/`], {
  stdio: ["pipe", "pipe", "ignore"],
});
const sendFrame = (message) => {
  const body = Buffer.from(JSON.stringify(message));
  const length = Buffer.alloc(4);
  length.writeUInt32LE(body.length);
  host.stdin.write(Buffer.concat([length, body]));
};
let hasReported = false;
let pending = Buffer.alloc(0);
host.stdout.on("data", (chunk) => {
  pending = Buffer.concat([pending, chunk]);
  while (pending.length >= 4 && pending.length >= 4 + pending.readUInt32LE(0)) {
    const length = pending.readUInt32LE(0);
    const message = JSON.parse(pending.subarray(4, 4 + length).toString("utf8"));
    pending = pending.subarray(4 + length);
    if (message.type !== "shortcut_press_expected") continue;
    sendFrame({ type: "shortcut_press_noted", session_id: message.session_id });
    console.log(JSON.stringify({ session_id: message.session_id, at_ms: Date.now() }));
    if (hasReported) continue;
    hasReported = true;
    setTimeout(
      () => sendFrame({ type: "shortcut_press_landed", session_id: message.session_id }),
      300,
    );
  }
});
host.on("exit", () => process.exit(0));
process.on("SIGTERM", () => {
  host.stdin.end();
  setTimeout(() => host.kill(), 2000);
});
sendFrame({ type: "hello", protocol: 1, extension_version: "0.0.0-second", sessions: [] });
