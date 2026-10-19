# Overview

A desktop tray app for Mac and Windows that turns every call I take into a meeting note in my
Obsidian vault. No bot joins the call. Everything runs on the laptop I take the call on.

## Problem

A self-hosted bot (Attendee, headless Chrome on a VM) joined Google Meet calls and wrote notes. It
broke often: dispatch, Chromium, the lobby or the VM failing loses the meeting. Since March 2026,
Meet also flags bots as risky and defaults the admit prompt to Deny. The bot wrote no note after
2026-09-28.

## Goals

- A transcript with speaker names for every call: Google Meet, Teams, Zoom, Slack huddles.
- After each call, a vault note with summary, decisions, action items and my commitments.
- Optional audio and video files of the call.
- Mac first (M2, 16 GB). Windows works too, with the same features.
- Light load during the call. Heavy work (speech-to-text) runs after the call ends.
- Running cost under $5 a month, transcription and AI included.

## Non-goals

- Recording calls I do not attend.
- A bot participant of any kind.
- Captions from calls in other browsers or in Teams and Zoom web. mc-meet reads Google Meet in
  Chromium browsers only; local capture covers the rest.
- Our own screen recorder. OBS records video; the app only starts and stops it.
- Reading or acting on the vault after the note is written. Other tools use the vault directly.
- Sharing the app with other people. It is built and signed for my own machines.

## What happens in one call

1. I join a call. The app sees an app start using the mic.
2. The app records two audio stems: my mic, and system audio (everyone else).
3. On Meet, the mc-meet extension streams captions, chat and the participant list, with real
   names, to the app.
4. The call ends. The app transcribes the stems locally and separates the remote speakers.
5. The app merges the sources. Caption names replace "Speaker 1".
6. It finds the matching Google Calendar event for title, invitees and client.
7. A cheap LLM extracts summary, decisions, action items and my commitments.
8. It writes `~/knowledge/areas/meetings/YYYY/MM/YYYY-MM-DD <event title>.md`.
9. Audio and video files go to a folder synced to Google Drive, never into the vault.

## Budget

| Item | Cost per month |
|---|---|
| Local speech-to-text (Parakeet) | $0 |
| Soniox async, Hinglish calls only, $0.10 per hour | About $1 |
| Extraction through Claude Code, on the existing Claude plan | $0 extra |
| Or Claude Haiku 5.5 extraction through the API | About $0.15 to $0.90 |
| Google Calendar API | $0 |
| Storage on personal Google Drive | $0 |
