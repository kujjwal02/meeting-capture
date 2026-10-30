# Driving Meet unattended: known traps

Each entry is a failure seen while driving real Meet calls from scripts, with its fix. Read this
before you write a new driver or spike.

## Accounts and joining

- Meet removes guests without a Google account. It sends them to the Workspace product page, in
  both patchrome and plain Chrome, for meetings that a personal account hosts. Join every guest
  signed in.
- Two browsers on the same account join, but Meet sends no audio between them. Any audio or echo
  test needs two different accounts.
- With the same account, "Switch here" is the primary button. Never click it: it moves the host
  out of the call. "Join here too" is a menu item under "Other ways to join".
- A "Got it" tip appears late and covers the join button, so clicks time out. Dismiss tips first.
  Click with a short timeout (`--timeout-ms 5000`) inside a retry loop.
- Search for join controls in both the `button` and `menuitem` roles. Accept "Join now",
  "Join here too" and "Ask to join".
- A disabled button has no ref in a patchrome snapshot. A guest's "Ask to join" stays disabled
  until the name box has text. Wait for the name box, fill it, then look for the button.
- The guest sees "Continue without microphone and camera" when it has no permission. Call
  `Browser.grantPermissions` with `audioCapture` and `videoCapture` before you navigate.
- After the join, the modal "Others may see your video differently" hides "Leave call". Dismiss
  "Got it" before you check that the browser is in the call.
- The host sees "Your meeting's ready" and a Gemini prompt. Both hide the admit toast. Dismiss
  them before you admit.
- A signed-in host on `meet.new` lands on `/meet/<code>`. Guests need `/<code>`.

## Browsers

- A patchrome daemon starts on the first command. `daemon_unreachable` means no page is open
  yet. Open a page first.
- `patchrome state save` copies only cookies for the sites the browser has visited. Visit
  `accounts.google.com` and `meet.google.com` before you save, or the copy cannot join.
- Patchrome starts Chrome with `--remote-debugging-port=0` and a pipe, so no fixed port exists.
  Find the main process by its `patchrome/<profile>/chrome-profile` user data folder. Read the
  DevTools URL from `patchrome daemon status`.
- Patchrome's Chrome runs with `--disable-background-timer-throttling`,
  `--disable-backgrounding-occluded-windows` and `--disable-renderer-backgrounding`. These flags
  hide throttling. Measure frame rates or timers in a hidden tab with plain Chrome.
- Start plain Chrome with its own `--user-data-dir`, `--remote-debugging-port`,
  `--enable-unsafe-extension-debugging`, `--no-first-run` and `--no-default-browser-check`.
  Sign it in with `Storage.setCookies` from a saved patchrome state. Load an unpacked extension
  with `Extensions.loadUnpacked` over the port.
- `Target.createTarget` opens a new window, so the Meet tab stays the active tab in its own
  window. To put another tab in front of Meet, call `chrome.tabs.create` from the extension's
  service worker.
- Delete copied cookies and throwaway profiles at the end of the run. They hold live Google
  sessions.

## Extensions and gestures

- `chrome.tabCapture.getMediaStreamId` needs an invocation gesture: the action click or a
  `commands` shortcut. Events from DevTools and clicks in the page do not count.
- `osascript` sends a real key event that Chrome accepts as the shortcut. First bring the
  browser to the front by process id, then send the keystroke:

  ```applescript
  tell application "System Events"
    set frontmost of (first process whose unix id is 12345) to true
    keystroke "r" using {option down, shift down}
  end tell
  ```

## Audio test signals

- Meet's noise suppression removes a pure tone. A 1 kHz beep arrived at a 0.0016 peak. Use a
  short spoken sample from `say`. It arrives at about 0.6 peak.
- Meet opens the microphone with `echoCancellation`, `noiseSuppression` and `autoGainControl` all
  false, then cancels echo inside the page. Metering a clone of the microphone track says nothing
  about the echo that other people hear. Measure echo at the far end.
- WebAudio on a received track reads zeros while Meet's media elements are muted. Read
  `totalAudioEnergy` from the `inbound-rtp` stats instead.
- Mute the sender's own media elements, so the system audio tap hears only the receiver.

## Timing and files

- A probe that is killed without finalize leaves a WAV header with wrong sizes. Read the samples
  after the `data` chunk and ignore the header sizes.
- Chrome stamps UTC and the Core Audio tap stamps host time (`mach_absolute_time`). Map one to
  the other with an anchor pair read together: `mach_absolute_time` and the system time.

## Keep runs from hanging

- Log each step with its time. A watchdog prints every browser's buttons after 60 s without a new
  step, so the stuck screen is visible in the output.
- Put a deadline on every wait, and clean up on any exit: stop browsers, the probe and the log
  server, and restore every file the run changed.
