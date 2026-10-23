// Caption source policy: picks the channel reader or the DOM reader from timestamped facts.
// It touches no DOM, no chrome.* API and no clock, so it also runs in Node. main.js feeds it
// facts, starts or stops the DOM reader to match the source it returns, and reports health.
//
// Facts, by kind:
//   call_began           { atMs } the page joined a call
//   call_ended           the call ended; the captions click budget starts again
//   session_dropped      the worker ended the session while the page was away
//   hook_state           { channelsOpenedAtMs } when the page hook opened the caption channels
//   channel_open_failed  { label, error } the page hook could not open a caption channel
//   channel_caption      { atMs } a caption arrived on a channel, in a call
//   parse_gap            { atMs, channel, parseFailures, detail, isInCall } frames failed to decode
//   tick                 { atMs } the content script's once-a-second check, in a call
//   captions_clicked     { atMs } main.js clicked Meet's captions button
//   captions_hidden      { atMs } main.js turned Meet's captions off; the channel keeps streaming
//
// decide returns { source, health, reportsHealth, isCaptionsClickDue, isCaptionsHideDue }. source is "none",
// "channel" or "dom". health holds the health event fields this policy owns.

// The DOM reader starts when no caption arrives this long after the caption channels open.
const MC_MEET_CAPTION_WAIT_MS = 20000;
const MC_MEET_CAPTIONS_CLICK_ATTEMPTS = 3;
const MC_MEET_CAPTIONS_CLICK_GAP_MS = 2000;

globalThis.mcMeetCreateCaptionSourcePolicy = () => {
  let source = "none";
  let parseFailures = 0;
  let detail = null;
  // Caption frames arrive but fail to decode, and no caption decoded since.
  let isCaptionFramesFailing = false;
  let pageStartedMs = 0;
  let channelsOpenedAtMs = null;
  let lastChannelCaptionMs = null;
  let captionsClicks = 0;
  let lastCaptionsClickMs = 0;
  // Once hidden, mc-meet never turns captions back on itself, so a user who wants them can.
  let isCaptionsHidden = false;
  // Hiding before the current channels carry a caption would stop them from ever starting.
  let hasCaptionOnOpenChannels = false;

  const health = () => ({ source, parseFailures, detail, isCaptionFramesFailing });

  // Returns whether the source changed, which is when health is due.
  function useDom(reason) {
    if (source === "dom") return false;
    source = "dom";
    detail = reason;
    captionsClicks = 0;
    // The DOM reader needs Meet's captions on.
    isCaptionsHidden = false;
    return true;
  }

  function useChannel() {
    if (source === "channel") return false;
    source = "channel";
    detail = null;
    return true;
  }

  function isCaptionsClickDue(atMs) {
    return (
      !isCaptionsHidden &&
      captionsClicks < MC_MEET_CAPTIONS_CLICK_ATTEMPTS &&
      atMs - lastCaptionsClickMs >= MC_MEET_CAPTIONS_CLICK_GAP_MS
    );
  }

  // Returns whether health is due.
  function apply(fact) {
    switch (fact.kind) {
      case "call_began":
        source = "none";
        pageStartedMs = fact.atMs;
        lastChannelCaptionMs = null;
        isCaptionsHidden = false;
        hasCaptionOnOpenChannels = false;
        isCaptionFramesFailing = false;
        return true;
      case "call_ended":
        source = "none";
        captionsClicks = 0;
        return false;
      case "session_dropped":
        source = "none";
        return false;
      case "hook_state":
        channelsOpenedAtMs = fact.channelsOpenedAtMs;
        // New channels on a new peer connection need Meet's captions on to start streaming.
        isCaptionsHidden = false;
        hasCaptionOnOpenChannels = false;
        isCaptionFramesFailing = false;
        // "channel" says a caption arrived on the open channels. The new ones have to show it
        // again, or the DOM reader takes over after the wait.
        if (source === "channel") source = "none";
        return true;
      case "channel_open_failed":
        detail = `opening ${fact.label} failed: ${fact.error}`;
        return true;
      case "channel_caption": {
        lastChannelCaptionMs = fact.atMs;
        hasCaptionOnOpenChannels = true;
        const wasFailing = isCaptionFramesFailing;
        isCaptionFramesFailing = false;
        return useChannel() || wasFailing;
      }
      case "parse_gap": {
        parseFailures = fact.parseFailures;
        detail = fact.detail;
        const isCaptionChannel = fact.channel === "captions" || fact.channel === "captions_v2";
        if (isCaptionChannel) isCaptionFramesFailing = true;
        const isChannelQuiet =
          lastChannelCaptionMs === null ||
          fact.atMs - lastChannelCaptionMs > MC_MEET_CAPTION_WAIT_MS;
        // An already running DOM reader reports the new failure count on the next tick.
        if (fact.isInCall && isCaptionChannel && isChannelQuiet) return useDom(fact.detail);
        return true;
      }
      case "tick": {
        const waitStartMs = Math.max(pageStartedMs, channelsOpenedAtMs ?? 0);
        if (source !== "none" || fact.atMs - waitStartMs < MC_MEET_CAPTION_WAIT_MS) return false;
        return useDom(
          channelsOpenedAtMs
            ? "no caption frame within 20 s of opening the caption channels"
            : "page hook did not open the caption channels",
        );
      }
      case "captions_clicked":
        captionsClicks += 1;
        lastCaptionsClickMs = fact.atMs;
        return false;
      case "captions_hidden":
        isCaptionsHidden = true;
        return false;
    }
  }

  return {
    decide(fact) {
      const reportsHealth = apply(fact);
      return {
        source,
        health: health(),
        reportsHealth,
        isCaptionsClickDue: fact.kind === "tick" && isCaptionsClickDue(fact.atMs),
        isCaptionsHideDue:
          fact.kind === "tick" &&
          source === "channel" &&
          hasCaptionOnOpenChannels &&
          !isCaptionsHidden,
      };
    },
    health,
  };
};
