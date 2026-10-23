// Replays failed Meet frames through extension/page/wire.js, so a decoder change can be checked
// against frames a real call could not read.
// Usage: node tools/replay-frames.mjs <debug report folder, debug folder or frames.jsonl>
// A report saved without caption text holds no frame bytes; its frames are listed as skipped.
import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const FRAMES_FILE = "frames.jsonl";
const repoDir = join(dirname(fileURLToPath(import.meta.url)), "..");

function framesPath(target) {
  if (!statSync(target).isDirectory()) return target;
  for (const candidate of [join(target, FRAMES_FILE), join(target, "debug", FRAMES_FILE)]) {
    if (existsSync(candidate)) return candidate;
  }
  throw new Error(`no ${FRAMES_FILE} in ${target}`);
}

function describeReading(reading) {
  switch (reading.kind) {
    case "caption":
      return `caption ${reading.utteranceId} v${reading.version} from ${reading.speaker || reading.deviceId}: ${JSON.stringify(reading.text)}`;
    case "chat":
      return `chat ${reading.messageId} from ${reading.sender}: ${JSON.stringify(reading.text)}`;
    case "roster":
      return `roster of ${reading.participants.length}`;
  }
  return `reading ${reading.kind}`;
}

async function main() {
  const target = process.argv[2];
  if (!target) {
    console.error("usage: node tools/replay-frames.mjs <report folder or frames.jsonl>");
    process.exit(2);
  }
  await import(pathToFileURL(join(repoDir, "extension", "page", "wire.js")).href);
  const wire = globalThis.mcMeetWire;

  let readings = [];
  let failures = [];
  // One reader for the whole file, as in the page: names from collections frames carry over.
  const reader = wire.createMeetReader({
    emit: (reading) => readings.push(reading),
    notice: (notice) => {
      if (notice.kind === "frame_failed") failures.push(notice);
    },
  });

  const path = framesPath(target);
  const lines = readFileSync(path, "utf8").split("\n").filter(Boolean);
  const tally = { decoded: 0, failing: 0, skipped: 0 };
  console.log(`${path}: ${lines.length} frames`);
  for (const [index, line] of lines.entries()) {
    const frame = JSON.parse(line);
    const heading = `#${index + 1} ${frame.channel} at ${frame.at}, was ${frame.reason}`;
    if (!frame.bytes_base64) {
      tally.skipped += 1;
      console.log(`${heading}: skipped, no bytes (${frame.byte_length} bytes, ${frame.skeleton})`);
      continue;
    }
    readings = [];
    failures = [];
    // The reader reports only the first failures per call; each replayed frame must report.
    reader.newCall();
    await reader.frame(frame.channel, Buffer.from(frame.bytes_base64, "base64"));
    if (failures.length > 0) {
      tally.failing += 1;
      const [failure] = failures;
      console.log(`${heading}: still fails, ${failure.reason}; skeleton ${failure.skeleton}`);
      continue;
    }
    tally.decoded += 1;
    const outcome = readings.length > 0 ? readings.map(describeReading).join("; ") : "no reading";
    console.log(`${heading}: decodes, ${outcome}`);
  }
  console.log(`decoded ${tally.decoded}, still failing ${tally.failing}, skipped ${tally.skipped}`);
}

await main();
