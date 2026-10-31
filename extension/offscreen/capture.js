// Offscreen document: opens the Meet tab's stream from the worker's stream id, plays its audio
// back (tab capture mutes the tab for the user), encodes its video with WebCodecs, muxes fMP4
// fragments with fmp4.js and sends each one to the worker. Offscreen documents have only
// chrome.runtime messaging, which JSON-serialises, so bytes travel as base64.

import { fragment, initSegment } from "./fmp4.js";

const FRAME_RATE = 15;
const FRAME_WIDTH_PX = 1280;
const FRAME_HEIGHT_PX = 720;
const KEYFRAME_INTERVAL_US = 2_000_000;
const MAX_ENCODE_QUEUE = 15;
const CLOCK_STEP_LIMIT_MS = 1000;
const TICKS_PER_US = 0.09;
const TICKS_PER_MS = 90;
const STOP_SAMPLE_TICKS = 90000 / FRAME_RATE;
const MAX_FRAGMENT_BASE64_CHARS = 7 * 1024 * 1024;
const BASE64_SLICE_BYTES = 32 * 1024;
const ENCODER_CANDIDATES = [
  {
    codec: "hevc",
    codecString: "hvc1.1.6.L93.B0",
    bitrateBps: 700_000,
    formatOption: { hevc: { format: "hevc" } },
  },
  {
    codec: "h264",
    codecString: "avc1.64001f",
    bitrateBps: 1_000_000,
    formatOption: { avc: { format: "avc" } },
  },
  {
    codec: "h264",
    codecString: "avc1.4d401f",
    bitrateBps: 1_000_000,
    formatOption: { avc: { format: "avc" } },
  },
];
const ACCELERATIONS = ["prefer-hardware", "no-preference"];

// The running capture, or null. Each async step checks it still owns the capture, since a stop or
// a failure can land while it waits.
let capture = null;

// UTC minus the monotonic clock. performance.now() stops while the Mac sleeps and Date.now() does
// not, so this grows by the sleep's length; it also moves when the wall clock is set.
const clockOffsetMs = () => Date.now() - performance.now();

function errorText(error) {
  return String(error?.message ?? error);
}

function tellWorker(message) {
  chrome.runtime.sendMessage({ target: "worker", ...message }).catch(() => {});
}

function sendState(current, state, detail) {
  tellWorker({ type: "video_state", session_id: current.sessionId, state, detail });
}

function base64Of(bytes) {
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += BASE64_SLICE_BYTES) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + BASE64_SLICE_BYTES));
  }
  return btoa(binary);
}

function copyOf(bufferSource) {
  if (ArrayBuffer.isView(bufferSource)) {
    return new Uint8Array(
      bufferSource.buffer.slice(
        bufferSource.byteOffset,
        bufferSource.byteOffset + bufferSource.byteLength,
      ),
    );
  }
  return new Uint8Array(bufferSource.slice(0));
}

async function pickEncoder(width, height) {
  for (const candidate of ENCODER_CANDIDATES) {
    for (const hardwareAcceleration of ACCELERATIONS) {
      const config = {
        codec: candidate.codecString,
        width,
        height,
        bitrate: candidate.bitrateBps,
        framerate: FRAME_RATE,
        latencyMode: "realtime",
        bitrateMode: "variable",
        hardwareAcceleration,
        ...candidate.formatOption,
      };
      const support = await VideoEncoder.isConfigSupported(config).catch(() => undefined);
      if (support?.supported) return { codec: candidate.codec, config };
    }
  }
  return null;
}

async function start({ stream_id: streamId, session_id: sessionId, capture_id: captureId }) {
  if (capture) throw new Error(`capture ${capture.captureId} is still running`);
  const source = { chromeMediaSource: "tab", chromeMediaSourceId: streamId };
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { mandatory: source },
    video: {
      mandatory: {
        ...source,
        // Equal bounds make Chrome letterbox into one size, so a window resize keeps the part.
        minWidth: FRAME_WIDTH_PX,
        maxWidth: FRAME_WIDTH_PX,
        minHeight: FRAME_HEIGHT_PX,
        maxHeight: FRAME_HEIGHT_PX,
        maxFrameRate: FRAME_RATE,
      },
    },
  });
  let audioContext = null;
  try {
    const [videoTrack] = stream.getVideoTracks();
    if (!videoTrack) throw new Error("the tab stream has no video track");
    if (stream.getAudioTracks().length > 0) {
      audioContext = new AudioContext({ latencyHint: "interactive" });
      audioContext.createMediaStreamSource(stream).connect(audioContext.destination);
      await audioContext.resume();
    }
    const current = {
      sessionId,
      captureId,
      stream,
      videoTrack,
      audioContext,
      reader: null,
      readingDone: Promise.resolve(),
      encoder: null,
      encoderConfig: null,
      codec: null,
      // The encoder's last decoder description, for the next part's init segment.
      description: null,
      part: null,
      partCount: 0,
      openFragment: null,
      fragmentIndex: 0,
      // The last index sent; an oversized fragment takes an index but is not sent.
      lastSentIndex: 0,
      lastKeyRequestUs: null,
      skippedFrameCount: 0,
      droppedFragmentCount: 0,
      isStopping: false,
      isTornDown: false,
    };
    capture = current;
    videoTrack.addEventListener("ended", () => endByTrack(current));
    current.readingDone = readFrames(current).catch((error) => fail(current, error));
    console.info("mc-meet video: stream open", {
      session_id: sessionId,
      capture_id: captureId,
      video: videoTrack.getSettings(),
      has_audio: audioContext !== null,
    });
  } catch (error) {
    for (const track of stream.getTracks()) track.stop();
    audioContext?.close().catch(() => {});
    throw error;
  }
}

async function readFrames(current) {
  const reader = new MediaStreamTrackProcessor({ track: current.videoTrack }).readable.getReader();
  current.reader = reader;
  for (;;) {
    const { value: frame, done } = await reader.read();
    if (done) return;
    try {
      if (!current.isStopping) await encodeFrame(current, frame);
    } finally {
      frame.close();
    }
    if (current.isStopping) return;
  }
}

async function encodeFrame(current, frame) {
  const arrivalMonoMs = performance.now();
  const offsetMs = clockOffsetMs();
  const widthPx = frame.displayWidth & ~1;
  const heightPx = frame.displayHeight & ~1;
  if (!current.encoder) {
    await openEncoder(current, widthPx, heightPx);
  } else if (widthPx !== current.encoderConfig.width || heightPx !== current.encoderConfig.height) {
    await endPart(current);
    console.info("mc-meet video: frame size changed", {
      from: [current.encoderConfig.width, current.encoderConfig.height],
      to: [widthPx, heightPx],
    });
    current.encoderConfig = { ...current.encoderConfig, width: widthPx, height: heightPx };
    current.encoder.configure(current.encoderConfig);
    current.description = null;
  } else if (
    current.part &&
    Math.abs(offsetMs - current.part.clockOffsetMs) > CLOCK_STEP_LIMIT_MS
  ) {
    // A sleep or a clock change. A sleep breaks the monotonic placement, so a new part starts
    // either way; the app decides which offset the new part keeps.
    const stepMs = Math.round(offsetMs - current.part.clockOffsetMs);
    await endPart(current);
    console.info("mc-meet video: the wall clock stepped", { step_ms: stepMs });
  }
  if (current.isStopping) return;
  if (!current.part) {
    current.partCount += 1;
    current.part = {
      number: current.partCount,
      // Fixed for the part: frames inside it are placed by monotonic time only.
      clockOffsetMs: offsetMs,
      firstArrivalMonoMs: arrivalMonoMs,
      firstTimestampUs: frame.timestamp,
      widthPx,
      heightPx,
      initBase64: null,
      isKeyPending: true,
    };
    console.info("mc-meet video: part", current.part.number, "at", [widthPx, heightPx]);
  }
  if (current.encoder.encodeQueueSize >= MAX_ENCODE_QUEUE) {
    current.skippedFrameCount += 1;
    return;
  }
  const isKeyRequested =
    current.part.isKeyPending || frame.timestamp - current.lastKeyRequestUs >= KEYFRAME_INTERVAL_US;
  if (isKeyRequested) {
    current.part.isKeyPending = false;
    current.lastKeyRequestUs = frame.timestamp;
  }
  current.encoder.encode(frame, { keyFrame: isKeyRequested });
}

async function openEncoder(current, widthPx, heightPx) {
  const chosen = await pickEncoder(widthPx, heightPx);
  if (!chosen) throw new Error(`no supported video encoder for ${widthPx}x${heightPx}`);
  if (current.isStopping) return;
  current.codec = chosen.codec;
  current.encoderConfig = chosen.config;
  current.encoder = new VideoEncoder({
    output: (chunk, metadata) => takeChunk(current, chunk, metadata),
    error: (error) => fail(current, error),
  });
  current.encoder.configure(chosen.config);
  console.info("mc-meet video: capturing", {
    codec: chosen.config.codec,
    hardware_acceleration: chosen.config.hardwareAcceleration,
    width_px: widthPx,
    height_px: heightPx,
  });
  sendState(current, "capturing", null);
}

// Waits for every frame of the part to come out of the encoder, then sends its open fragment.
async function endPart(current) {
  await current.encoder.flush();
  if (current.openFragment) closeFragment(current, null);
  current.part = null;
}

function takeChunk(current, chunk, metadata) {
  if (current.isTornDown) return;
  try {
    if (metadata?.decoderConfig?.description) {
      current.description = copyOf(metadata.decoderConfig.description);
    }
    const part = current.part;
    if (!part) return;
    const isKey = chunk.type === "key";
    if (!part.initBase64) {
      // A part must start on a keyframe so it decodes on its own.
      if (!isKey) return;
      if (!current.description) throw new Error("the encoder gave no decoder description");
      part.initBase64 = base64Of(
        initSegment({
          codec: current.codec,
          width: part.widthPx,
          height: part.heightPx,
          description: current.description,
        }),
      );
    }
    if (isKey && current.openFragment) closeFragment(current, chunk.timestamp);
    const bytes = new Uint8Array(chunk.byteLength);
    chunk.copyTo(bytes);
    current.openFragment ??= { samples: [] };
    current.openFragment.samples.push({ bytes, timestampUs: chunk.timestamp, isKey });
  } catch (error) {
    fail(current, error);
  }
}

// nextTimestampUs: the next keyframe's timestamp, or null at a part end or stop.
function closeFragment(current, nextTimestampUs) {
  const part = current.part;
  const { samples } = current.openFragment;
  current.openFragment = null;
  // Ticks from the part's start for each sample, so rounding does not add up over a part.
  const ticksOf = (timestampUs) => Math.round((timestampUs - part.firstTimestampUs) * TICKS_PER_US);
  const startTicks = samples.map((sample) => ticksOf(sample.timestampUs));
  const lastSampleEndTicks =
    nextTimestampUs === null ? startTicks.at(-1) + STOP_SAMPLE_TICKS : ticksOf(nextTimestampUs);
  const muxedSamples = samples.map((sample, position) => {
    const endTicks = position + 1 < samples.length ? startTicks[position + 1] : lastSampleEndTicks;
    return {
      bytes: sample.bytes,
      isKey: sample.isKey,
      durationTicks: Math.max(0, endTicks - startTicks[position]),
    };
  });
  current.fragmentIndex += 1;
  const bytesBase64 = base64Of(
    fragment({
      sequenceNumber: current.fragmentIndex,
      baseDecodeTicks: startTicks[0],
      samples: muxedSamples,
    }),
  );
  if (bytesBase64.length > MAX_FRAGMENT_BASE64_CHARS) {
    current.droppedFragmentCount += 1;
    console.warn("mc-meet video: dropped an oversized fragment", {
      index: current.fragmentIndex,
      base64_chars: bytesBase64.length,
      dropped_count: current.droppedFragmentCount,
    });
    return;
  }
  const durationTicks = muxedSamples.reduce((sum, sample) => sum + sample.durationTicks, 0);
  const outputLatencyS = current.audioContext?.outputLatency;
  const baseLatencyS = current.audioContext?.baseLatency;
  const roundedMs = (seconds) => (seconds === undefined ? null : Math.round(seconds * 1e4) / 10);
  tellWorker({
    type: "video_fragment",
    session_id: current.sessionId,
    capture_id: current.captureId,
    part: part.number,
    codec: current.codec,
    width_px: part.widthPx,
    height_px: part.heightPx,
    init_base64: part.initBase64,
    index: current.fragmentIndex,
    first_frame_utc_ms:
      part.clockOffsetMs +
      part.firstArrivalMonoMs +
      (samples[0].timestampUs - part.firstTimestampUs) / 1000,
    duration_ms: durationTicks / TICKS_PER_MS,
    frame_count: samples.length,
    // The app keeps a part on its earlier offset when the step came with no sleep, since the
    // audio keeps its clock reading then.
    part_clock_offset_ms: part.clockOffsetMs,
    clock_offset_ms: clockOffsetMs(),
    playback_latency_ms: roundedMs(outputLatencyS),
    playback_base_latency_ms: roundedMs(baseLatencyS),
    bytes_base64: bytesBase64,
  });
  current.lastSentIndex = current.fragmentIndex;
}

// Sends what the encoder still holds, then releases the stream. Resolves to the last fragment
// index sent, 0 when none was.
async function finish(current) {
  current.isStopping = true;
  await current.reader?.cancel().catch(() => {});
  await current.readingDone;
  if (current.encoder?.state === "configured") {
    await current.encoder.flush().catch((error) => {
      console.warn("mc-meet video: encoder flush failed:", errorText(error));
    });
  }
  if (!current.isTornDown && current.openFragment) closeFragment(current, null);
  tearDown(current);
  return current.lastSentIndex;
}

// Releases the stream and drops the open fragment and what the encoder holds: after the tab left
// Meet, its last frames may show another page.
function discard(current) {
  tearDown(current);
  return current.lastSentIndex;
}

function tearDown(current) {
  if (current.isTornDown) return;
  current.isTornDown = true;
  current.isStopping = true;
  current.reader?.cancel().catch(() => {});
  for (const track of current.stream.getTracks()) track.stop();
  if (current.encoder && current.encoder.state !== "closed") current.encoder.close();
  current.audioContext?.close().catch(() => {});
  if (capture === current) capture = null;
  console.info("mc-meet video: capture closed", {
    capture_id: current.captureId,
    fragments: current.fragmentIndex,
    skipped_frames: current.skippedFrameCount,
    dropped_fragments: current.droppedFragmentCount,
  });
}

function fail(current, error) {
  if (current.isTornDown) return;
  const text = errorText(error);
  console.info("mc-meet video: failed:", text);
  tearDown(current);
  sendState(current, "failed", text);
}

async function endByTrack(current) {
  if (current.isStopping) return;
  console.info("mc-meet video: the tab's video track ended");
  const lastIndex = await finish(current);
  tellWorker({
    type: "video_state",
    session_id: current.sessionId,
    state: "stopped",
    detail: "track_ended",
    capture_id: current.captureId,
    last_index: lastIndex,
  });
}

// Resolves to { capture_id, last_index } of the capture it ended, or null with none running.
async function stop(reason) {
  const current = capture;
  if (!current) return null;
  console.info("mc-meet video: stopping:", reason);
  const lastIndex = reason === "tab_gone" ? discard(current) : await finish(current);
  return { capture_id: current.captureId, last_index: lastIndex };
}

chrome.runtime.onMessage.addListener((message, sender, reply) => {
  if (message?.target !== "offscreen" || sender.id !== chrome.runtime.id) return;
  switch (message.type) {
    case "start":
      start(message).then(
        () => reply({ ok: true }),
        (error) => reply({ ok: false, error: errorText(error) }),
      );
      return true;
    case "stop":
      stop(message.reason).then((ended) => reply({ ok: true, ended }));
      return true;
    case "status":
      reply(
        capture
          ? { capture_id: capture.captureId, session_id: capture.sessionId }
          : { capture_id: null },
      );
      return;
  }
});
