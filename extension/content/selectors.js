// Every DOM selector the extension uses, in one table. Prefer roles, aria attributes, data
// attributes and Material icon names: Meet renames its generated class names with each build.
// Icon names are ligature text inside an <i>, and do not change with the UI language.
globalThis.mcMeetSelectors = Object.freeze({
  // Icon text of the Leave call button. Its presence means the page is in a call.
  leaveCallIcon: "call_end",
  // Icon text of the captions button while captions are off, and while they are on.
  captionsOffIcon: "closed_caption_off",
  captionsOnIcon: "closed_caption",
  // Icon text of the "Captions and translation" button that replaces the captions button in
  // some accounts' Meet. It opens a popup; its aria-pressed says whether captions are on.
  captionsMenuIcon: "translate_cc",
  // Meet's hover tooltips. The captions button's one says when Meet refuses captions.
  tooltip: '[role="tooltip"]',
  // The text Meet shows on a disabled captions button, in English: "Live captions are
  // temporarily unavailable". Other UI languages rely on the disabled state alone.
  captionsUnavailableText: /captions (are )?(temporarily )?unavailable/i,
  // Elements that hold icon ligatures.
  icon: "i.google-symbols, i.material-icons-extended, i.material-symbols-outlined, i[aria-hidden='true']",
  // The caption region. Its children are speaker blocks: a name node, then a text node.
  captionRegion: 'div[role="region"][tabindex="0"], div[role="region"][aria-label="Captions"]',
  // People panel rows, when the user has the panel open.
  peopleRow: '[role="list"] [role="listitem"][aria-label]',
  // Video tiles. data-self-name is set on the local user's tile.
  participantTile: "[data-participant-id]",
  selfName: "[data-self-name]",
  // The meeting title, when Meet shows it.
  meetingTitle: "[data-meeting-title]",
});

// Returns the button (or role=button element) that holds the icon with this exact text.
globalThis.mcMeetFindIconButton = (iconText) => {
  const selectors = globalThis.mcMeetSelectors;
  for (const icon of document.querySelectorAll(selectors.icon)) {
    if (icon.textContent.trim() !== iconText) continue;
    const button = icon.closest("button, [role='button']");
    if (button) return button;
  }
  return null;
};
