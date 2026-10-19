# native-host

Host mode for the mc-meet extension. Chrome starts `meeting-capture chrome-extension://<id>/`, and
this crate relays native messaging frames between Chrome's stdio and the tray app's local socket.
It also writes the native host manifest that tells Chrome where the app is.

`docs/09-meet-extension.md` has the protocol.

## Origin

Started from Anarlog's `crates/chrome-native-host`:

- Upstream: <https://github.com/fastrepl/anarlog>
- Commit: `6fd155aa1af39ef652b767133caccb280de750f6`
- Licence: MIT, copied in `LICENSE`.

Kept: the frame reader, with its length check. Dropped: the state file writing and the meeting
state parsing. Added: the relay to the app socket, the app launch, and the host manifest.
