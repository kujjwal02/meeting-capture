// DOM readers: the caption fallback and the participant fallback.
//
// The caption reader is adapted from TranscripTonic (MIT), commit
// 0cb5eb5667759cd9cd0bb98d1ce6bdb763a30151:
// - extension/content-scripts/google-meet/index.js, transcriptMutationCallbackGoogleMeet
// - extension/content-scripts/common-utils.js, startTranscriptMonitor
// Licence: extension/content/LICENSE-transcriptonic.
// Changes: each text change is sent at once as a versioned caption event; a speaker turn is
// one utterance; no storage, side panel or telemetry.

// Meet trims a long monologue and starts over. A drop this large starts a new turn.
const MC_MEET_TRIM_CHARS = 250;
const MC_MEET_REGION_CHECK_MS = 2000;

globalThis.mcMeetCreateDomCaptions = ({ emit, selfName }) => {
  const selectors = globalThis.mcMeetSelectors;
  // Utterance IDs must not repeat after a reload of the same session.
  const pageToken = Math.random().toString(36).slice(2, 8);
  const turnCounts = new Map();
  let observer = null;
  let region = null;
  let monitorTimer = null;
  let turn = null;

  function resolveSpeaker(label) {
    // Meet labels the local user "You" in English. The roster name replaces it when known.
    return label === "You" ? selfName() || label : label;
  }

  function startTurn(element, speaker, text) {
    const count = (turnCounts.get(speaker) ?? 0) + 1;
    turnCounts.set(speaker, count);
    turn = {
      element,
      speaker,
      utteranceId: `dom-${pageToken}-${count}`,
      version: 0,
      text: "",
      atMs: Date.now(),
    };
    updateTurn(text);
  }

  function updateTurn(text) {
    if (text === turn.text) return;
    turn.text = text;
    turn.version += 1;
    emit({
      kind: "caption",
      at: new Date(turn.atMs).toISOString(),
      device_id: turn.speaker,
      utterance_id: turn.utteranceId,
      version: turn.version,
      speaker: turn.speaker,
      text,
      source: "dom",
    });
  }

  function onMutations(mutations) {
    for (const mutation of mutations) {
      if (mutation.type !== "characterData") continue;
      const textElement = mutation.target.parentElement;
      const blocks = [...(textElement?.parentElement?.parentElement?.children ?? [])];
      // The last two children are not captions. Earlier blocks get small late corrections that
      // would repeat lines, so only the block third from the end counts.
      if (blocks[blocks.length - 3] !== textElement?.parentElement) continue;
      const label = textElement?.previousSibling?.textContent;
      const text = textElement?.textContent;
      if (!label || !text) {
        turn = null;
        continue;
      }
      const speaker = resolveSpeaker(label);
      if (!turn || turn.element !== textElement) {
        startTurn(textElement, speaker, text);
      } else if (text.length - turn.text.length < -MC_MEET_TRIM_CHARS) {
        startTurn(textElement, speaker, text);
      } else {
        updateTurn(text);
      }
    }
  }

  function attachToRegion() {
    const node = document.querySelector(selectors.captionRegion);
    if (!node || node === region) return;
    observer?.disconnect();
    region = node;
    turn = null;
    observer = new MutationObserver(onMutations);
    observer.observe(node, { childList: true, characterData: true, subtree: true });
  }

  return {
    start() {
      if (monitorTimer !== null) return;
      attachToRegion();
      monitorTimer = setInterval(attachToRegion, MC_MEET_REGION_CHECK_MS);
    },
    stop() {
      if (monitorTimer !== null) clearInterval(monitorTimer);
      monitorTimer = null;
      observer?.disconnect();
      observer = null;
      region = null;
      turn = null;
    },
    isAttached: () => region !== null,
  };
};

// Reads participants from the People panel when it is open, else from the video tiles.
// Used only while the collections channel gives no roster.
globalThis.mcMeetReadDomRoster = () => {
  const selectors = globalThis.mcMeetSelectors;
  const byName = new Map();
  for (const row of document.querySelectorAll(selectors.peopleRow)) {
    const name = row.getAttribute("aria-label")?.trim();
    if (name) byName.set(name, { device_id: name, name, is_self: false });
  }
  if (byName.size === 0) {
    for (const tile of document.querySelectorAll(selectors.participantTile)) {
      const selfName = tile.querySelector(selectors.selfName)?.getAttribute("data-self-name");
      const name = (selfName || tile.getAttribute("aria-label") || "").trim();
      if (!name) continue;
      byName.set(name, {
        device_id: tile.getAttribute("data-participant-id") || name,
        name,
        is_self: Boolean(selfName),
      });
    }
  }
  return [...byName.values()].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
};
