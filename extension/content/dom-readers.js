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
  let isRegionHidden = false;

  // Meet keeps writing captions into a hidden region, so the reader works with the box gone.
  function showRegion() {
    if (region) region.style.display = isRegionHidden ? "none" : "";
  }

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
    emit(
      globalThis.mcMeetCallEvents.caption({
        atMs: turn.atMs,
        deviceId: turn.speaker,
        utteranceId: turn.utteranceId,
        version: turn.version,
        speaker: turn.speaker,
        text,
        source: "dom",
      }),
    );
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
    showRegion();
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
      if (region) region.style.display = "";
      region = null;
      turn = null;
    },
    setRegionHidden(isHidden) {
      isRegionHidden = isHidden;
      showRegion();
    },
    isAttached: () => region !== null,
  };
};

// Reads participants from the People panel when it is open, else from the video tiles.
// Used only while the collections channel gives no roster.
globalThis.mcMeetReadDomRoster = () => {
  const selectors = globalThis.mcMeetSelectors;
  const callEvents = globalThis.mcMeetCallEvents;
  const byName = new Map();
  for (const row of document.querySelectorAll(selectors.peopleRow)) {
    const name = row.getAttribute("aria-label")?.trim();
    if (name)
      byName.set(name, callEvents.rosterParticipant({ deviceId: name, name, isSelf: false }));
  }
  if (byName.size === 0) {
    for (const tile of document.querySelectorAll(selectors.participantTile)) {
      const selfName = tile.querySelector(selectors.selfName)?.getAttribute("data-self-name");
      const name = (selfName || tile.getAttribute("aria-label") || "").trim();
      if (!name) continue;
      byName.set(
        name,
        callEvents.rosterParticipant({
          deviceId: tile.getAttribute("data-participant-id") || name,
          name,
          isSelf: Boolean(selfName),
        }),
      );
    }
  }
  return [...byName.values()].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
};

// The last "captions unavailable" text a hover tooltip showed, for page snapshots.
let mcMeetUnavailableTooltip = null;

// Reads Meet's captions button: { isOn, isUnavailable, isFound, tooltip }. Meet refuses captions
// at times ("Live captions are temporarily unavailable"): it disables the button, or says so in
// the button's own label. The state rests on those, which hold while the page shows the button.
// A hover tooltip shows only under the pointer, so it would flip the state with each hover; it
// is kept for the snapshot only.
globalThis.mcMeetReadCaptionsButton = () => {
  const selectors = globalThis.mcMeetSelectors;
  const findIconButton = globalThis.mcMeetFindIconButton;
  const sayUnavailable = (text) =>
    text && selectors.captionsUnavailableText.test(text) ? text : null;
  const hoverText = [...document.querySelectorAll(selectors.tooltip)]
    .map((tooltip) => sayUnavailable(tooltip.textContent))
    .find(Boolean);
  if (hoverText) mcMeetUnavailableTooltip = hoverText.trim();
  const onButton = findIconButton(selectors.captionsOnIcon);
  const menuButton = findIconButton(selectors.captionsMenuIcon);
  const button = onButton ?? findIconButton(selectors.captionsOffIcon) ?? menuButton;
  if (!button) {
    return { isOn: false, isUnavailable: false, isFound: false, tooltip: mcMeetUnavailableTooltip };
  }
  const describedBy = button.getAttribute("aria-describedby");
  const ownText = [
    button.getAttribute("aria-label"),
    button.getAttribute("data-tooltip"),
    button.getAttribute("title"),
    describedBy ? document.getElementById(describedBy)?.textContent : null,
  ]
    .map(sayUnavailable)
    .find(Boolean);
  if (ownText) mcMeetUnavailableTooltip = ownText.trim();
  const isDisabled = button.disabled === true || button.getAttribute("aria-disabled") === "true";
  const isUnavailable = isDisabled || Boolean(ownText);
  return {
    isOn:
      (Boolean(onButton) ||
        (button === menuButton && button.getAttribute("aria-pressed") === "true")) &&
      !isUnavailable,
    isUnavailable,
    isFound: true,
    tooltip: mcMeetUnavailableTooltip,
  };
};

const MC_MEET_REGION_DEPTH = 5;
const MC_MEET_REGION_NODES = 60;

// Whether Meet's caption box takes up space on screen. Meet's captions button can read on with
// no box, and mc-meet's hide_captions sets the box to display none.
globalThis.mcMeetIsCaptionBoxShown = () => {
  const region = document.querySelector(globalThis.mcMeetSelectors.captionRegion);
  if (!region) return false;
  const box = region.getBoundingClientRect();
  return box.width > 0 && box.height > 0;
};

// The caption region's element tree without its text: tags, roles and the length of each text
// node. Shows how Meet laid out the region when the DOM reader reads nothing.
globalThis.mcMeetDescribeCaptionRegion = () => {
  const region = document.querySelector(globalThis.mcMeetSelectors.captionRegion);
  if (!region) return null;
  let nodes = 0;
  function describe(element, depth) {
    nodes += 1;
    const role = element.getAttribute("role");
    const ownTextChars = [...element.childNodes]
      .filter((child) => child.nodeType === Node.TEXT_NODE)
      .reduce((sum, child) => sum + child.textContent.length, 0);
    const shape = { tag: element.tagName.toLowerCase(), text_chars: ownTextChars };
    if (role) shape.role = role;
    const children = [...element.children];
    if (depth < MC_MEET_REGION_DEPTH && nodes < MC_MEET_REGION_NODES) {
      shape.children = children
        .filter(() => nodes < MC_MEET_REGION_NODES)
        .map((child) => describe(child, depth + 1));
    } else if (children.length > 0) {
      shape.hidden_children = children.length;
    }
    return shape;
  }
  return describe(region, 0);
};
