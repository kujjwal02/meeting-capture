// Caption source policy: picks the channel reader or the DOM reader from timestamped facts.
// It touches no DOM, no chrome.* API and no clock, so it also runs in Node. main.js feeds it
// facts, starts or stops the DOM reader to match the source it returns, and reports health.
//
// Facts, by kind:
//   call_began           { atMs } the page joined a call
//   call_ended           the call ended
//   session_dropped      the worker ended the session while the page was away
//   hook_state           { channelsOpenedAtMs } when the page hook opened the caption channels
//   channel_open_failed  { label, error } the page hook could not open a caption channel
//   channel_caption      { atMs } a caption arrived on a channel, in a call
//   parse_gap            { atMs, channel, parseFailures, detail, isInCall } frames failed to decode
//   tick                 { atMs } the content script's once-a-second check, in a call
//   captions_requested   { atMs } main.js clicked Meet's control that turns captions on
//
// decide returns { source, health, reportsHealth, isCaptionsRequestDue }. source is "none",
// "channel" or "dom". health holds the health event fields this policy owns.
//
// Meet streams nothing on the hook's caption channels until something asks for captions. A click
// on Meet's captions control asks the server, and the stream goes to the hook's channels. While
// the hook's captions_v2 is open, Meet renders no caption box. About 30 s later Meet's captions
// button reads on, still with no box. A second press stops the stream, so the policy asks for
// exactly one press per call.

// The DOM reader starts when no caption arrives this long after the caption channels open.
const MC_MEET_CAPTION_WAIT_MS = 20000;

globalThis.mcMeetCreateCaptionSourcePolicy = () => {
  let source = "none";
  let parseFailures = 0;
  let detail = null;
  // Caption frames arrive but fail to decode, and no caption decoded since.
  let isCaptionFramesFailing = false;
  let pageStartedMs = 0;
  let channelsOpenedAtMs = null;
  let lastChannelCaptionMs = null;
  // main.js pressed Meet's captions control in this call.
  let isCaptionsRequested = false;

  const health = () => ({ source, parseFailures, detail, isCaptionFramesFailing });

  // Returns whether the source changed, which is when health is due.
  function useDom(reason) {
    if (source === "dom") return false;
    source = "dom";
    detail = reason;
    return true;
  }

  function useChannel() {
    if (source === "channel") return false;
    source = "channel";
    detail = null;
    return true;
  }

  // Returns whether health is due.
  function apply(fact) {
    switch (fact.kind) {
      case "call_began":
        source = "none";
        pageStartedMs = fact.atMs;
        channelsOpenedAtMs = null;
        lastChannelCaptionMs = null;
        isCaptionsRequested = false;
        isCaptionFramesFailing = false;
        return true;
      case "call_ended":
        source = "none";
        return false;
      case "session_dropped":
        source = "none";
        return false;
      case "hook_state":
        channelsOpenedAtMs = fact.channelsOpenedAtMs;
        return true;
      case "channel_open_failed":
        detail = `opening ${fact.label} failed: ${fact.error}`;
        return true;
      case "channel_caption": {
        lastChannelCaptionMs = fact.atMs;
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
      case "captions_requested":
        isCaptionsRequested = true;
        return false;
      case "tick": {
        const waitStartMs = Math.max(pageStartedMs, channelsOpenedAtMs ?? 0);
        if (source !== "none" || fact.atMs - waitStartMs < MC_MEET_CAPTION_WAIT_MS) return false;
        return useDom(
          channelsOpenedAtMs
            ? "no caption frame within 20 s of opening the caption channels"
            : "page hook did not open the caption channels",
        );
      }
    }
  }

  return {
    decide(fact) {
      const reportsHealth = apply(fact);
      return {
        source,
        health: health(),
        reportsHealth,
        isCaptionsRequestDue:
          // Before the hook's channels open, a request would turn Meet's caption box on.
          fact.kind === "tick" &&
          channelsOpenedAtMs !== null &&
          source !== "channel" &&
          !isCaptionsRequested,
      };
    },
    health,
  };
};
