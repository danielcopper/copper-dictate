# Security Policy

## Supported Versions

| Version | Supported |
| ------- | --------- |
| Latest  | Yes       |
| Older   | No        |

Only the latest release receives security fixes.

## Reporting a Vulnerability

If you discover a security vulnerability in copper-dictate, please report it responsibly:

1. **Do NOT open a public GitHub issue.**
2. Use [GitHub Security Advisories](https://github.com/danielcopper/copper-dictate/security/advisories/new) to report
   privately.
3. Include:
   - Description of the vulnerability
   - Steps to reproduce
   - Potential impact
   - Suggested fix (if any)

You should receive a response within 7 days.

## Scope

copper-dictate runs as a daemon under your user and records from the default microphone only between two `toggle` calls.

- **Audio** stays in memory for the current dictation only and is never written to disk. When the recording stops, it is
  sent once, as 16 kHz mono WAV, to the transcription endpoint in your configuration, and dropped once the answer is
  back.
- **Network connections** go to that endpoint only: proxy variables are ignored, redirects are not followed, and there
  is no telemetry. With an `http://` endpoint, the audio and the key travel unencrypted.
- **The API key** is read from the file `api_key_file` names or from `COPPER_DICTATE_FINAL_API_KEY`, never from
  `config.toml`, and is sent only as the `Authorization` header of that request.
- **The socket** between `toggle` and the daemon lies in `$XDG_RUNTIME_DIR/copper-dictate/`, a directory only your user
  can open. The final text, or the error that ended the dictation, goes to the `toggle` that asked for it; the daemon
  keeps it only until a `toggle` takes it.
- **Insertion at the cursor** (KDE Plasma on Wayland): the final text, without its control characters, is set as the
  clipboard and the primary selection through the compositor's `ext-data-control`, marked so Klipper keeps no copy. Any
  application on the desktop can read it there until the previous contents come back, at most `restore_timeout_ms` after
  the paste key. Those previous contents (the types and the limit [the README](README.md#insertion-at-the-cursor) names)
  stay in the daemon's memory, offered again, until another application sets that selection; contents marked for
  password managers are neither kept nor offered again.
- **Remote control**: the paste key goes through the desktop's RemoteDesktop portal, over the session bus, as a session
  that may emulate a keyboard while the daemon runs; the daemon sends Shift+Insert and no other key. With "Allow
  restoring on future sessions", the portal's token lies in the state directory
  [the README](README.md#insertion-at-the-cursor) names, a file and directory only your user can open; it lets a later
  start open a session without asking. Without the file, the next start asks again. After the session was ended on
  purpose (its "End", or the portal closing it) or the permission was denied, the daemon deletes the file, so the next
  dictation asks again, after a restart as well; a deletion that fails is said on stderr, and the file left behind may
  let the daemon's next start restore the permission without asking.
