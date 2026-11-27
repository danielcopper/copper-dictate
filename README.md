<div align="center">

# copper-dictate

<h3>Self-hosted live dictation for Linux desktops</h3>

[![CI](https://github.com/danielcopper/copper-dictate/actions/workflows/ci.yml/badge.svg)](https://github.com/danielcopper/copper-dictate/actions/workflows/ci.yml)
[![Quality Gate](https://sonarcloud.io/api/project_badges/measure?project=danielcopper_copper-dictate&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=danielcopper_copper-dictate)
[![Coverage](https://img.shields.io/sonar/coverage/danielcopper_copper-dictate?server=https%3A%2F%2Fsonarcloud.io)](https://sonarcloud.io/summary/new_code?id=danielcopper_copper-dictate)
[![Maintainability](https://sonarcloud.io/api/project_badges/measure?project=danielcopper_copper-dictate&metric=sqale_rating)](https://sonarcloud.io/summary/new_code?id=danielcopper_copper-dictate)
[![Reliability](https://sonarcloud.io/api/project_badges/measure?project=danielcopper_copper-dictate&metric=reliability_rating)](https://sonarcloud.io/summary/new_code?id=danielcopper_copper-dictate)
[![Security](https://sonarcloud.io/api/project_badges/measure?project=danielcopper_copper-dictate&metric=security_rating)](https://sonarcloud.io/summary/new_code?id=danielcopper_copper-dictate)
[![License: MIT](https://img.shields.io/github/license/danielcopper/copper-dictate)](LICENSE)

</div>

Self-hosted live dictation for Linux desktops: live preview while you speak, an accurate final text when you stop.

> **Pre-1.0 (v0.x).** Only the final text works yet: a daemon records from the default microphone when you run
> `copper-dictate toggle`, and the second `toggle` inserts the text your transcription server returned at the cursor
> (KDE Plasma on Wayland) and prints it. The live preview, the shortcut and the overlay are planned, not features.

---

> [!NOTE]
> **How this is built.** The code is written by an AI coding agent working under my direction. The architecture, the
> design decisions and the review are mine, and changes are smoke-tested on real hardware before they ship.

## What works

- **A client daemon in Rust** on your desktop, recording from the default microphone (ALSA, so it needs `libasound` —
  every Linux desktop has it)
- **Accurate final text** — when you stop, the whole recording goes once to a self-hosted OpenAI-compatible
  transcription server (`/v1/audio/transcriptions`) as 16 kHz mono WAV, with automatic language detection unless the
  server has a default language of its own
- **Insertion at the cursor on KDE Plasma (Wayland)** — the final text is pasted into the focused window through the
  clipboard, so umlauts and every keyboard layout work, and the clipboard gets its previous content back
- **Nothing but the dictation leaves your machine** — no telemetry, no network connection but the configured endpoint
  (proxy variables are ignored, redirects are not followed), and the recording is dropped once its text is back

## Planned

- **Live preview** — the text appears while you speak, from a self-hosted realtime WebSocket server
- **A global shortcut and an overlay** — KDE Plasma first, behind interfaces

## Usage

Write the configuration to `$XDG_CONFIG_HOME/copper-dictate/config.toml` (`~/.config/copper-dictate/config.toml` when
the variable is unset):

```toml
[final]
endpoint = "http://whisper.lan:8000/v1/audio/transcriptions"
model = "primeline/whisper-large-v3-turbo-german"
# api_key_file = "/home/me/.config/copper-dictate/final.key"

[recording]
max_seconds = 900 # a recording stops by itself after this long; 15 minutes when left out

[insertion]
restore_timeout_ms = 1000 # the previous clipboard comes back after this long when nothing pastes; 1000 when left out
```

The endpoint is the full URL, `http` or `https`. The API key never goes into this file. It is read from the file
`api_key_file` names (a relative path counts from the configuration file's directory), or, without that line, from the
environment variable `COPPER_DICTATE_FINAL_API_KEY`; with neither, or with the variable empty, the request carries no
key. A key file that is empty, or a key with spaces, line breaks or characters outside visible ASCII, stops the daemon
at start.

```sh
copper-dictate daemon   # keep it running; it stops at once, naming the path and the field, if the configuration is incomplete
copper-dictate toggle   # start a recording
copper-dictate toggle   # stop it; inserts the final text at the cursor and prints it
```

`toggle` talks to the daemon over `$XDG_RUNTIME_DIR/copper-dictate/daemon.sock`. A recording that reaches `max_seconds`
stops and is transcribed by itself; the next `toggle` prints its text instead of starting a new recording. The same
holds for the text, or the error, of a `toggle` that was interrupted before it arrived. A microphone glitch the audio
stack recovers from (an overrun, a device change) does not end the recording; the `toggle` that prints its text also
warns on stderr how often the microphone glitched. A refused real-time priority loses no audio and is only logged. Any
other microphone error fails the dictation. The final server gets twice the recording's length to answer, at least ten
minutes; a server that takes longer fails the dictation. The daemon opens the microphone at 16 kHz where the device
offers that with a 16-bit, 32-bit or float sample format, and resamples the device's default configuration otherwise; it
logs the configuration it opened at the start of each recording.

### Insertion at the cursor

On KDE Plasma (Wayland), the daemon inserts the final text at the cursor of the focused window, and `toggle` still
prints it. Line breaks in the text become spaces and other control characters are dropped, so a dictation never presses
Enter or sends an escape sequence; a text of whitespace alone is not pasted. The daemon sets the text as the clipboard
and the primary selection, marked so Klipper keeps no copy, and sends Shift+Insert, which pastes in terminals and in GUI
text fields alike. It then puts back what the clipboard and the primary selection held before (text, HTML, file lists
and one image type, up to 64 MiB per selection), 200 ms after the window read the text or after `restore_timeout_ms`
when nothing read it. Contents a password manager marked are not kept, so that selection is left empty, and a selection
another application set in the meantime is left as it is. Of the types it keeps, one that does not come back is named on
stderr with the reason.

The key goes through the desktop's remote-control portal. At the first start Plasma asks for the permission in a "Remote
Control" dialog; with "Allow restoring on future sessions" ticked, later starts only show a notification, as the daemon
keeps the portal's token in `$XDG_STATE_HOME/copper-dictate/remote-desktop-token` (`~/.local/state/copper-dictate/` when
the variable is unset). Without a Wayland session, without `ext-data-control` on the compositor, or with the portal
missing, it says once on stderr why it cannot insert and keeps dictating, with the text on `toggle`'s output only; a
final text that ends while the daemon's first remote-control session starts is not inserted either. When one insertion
fails, the daemon puts the previous contents back where it can, says on stderr what failed and whether the paste key had
been sent (then the text may be at the cursor all the same), and tries again with the next dictation. If the
remote-control session has ended, that dictation opens a new one with the saved token. Plasma shows no "End" for this
session in its tray: the remote control ends when the daemon stops. After the portal ended the session itself (when it
restarts, for one) or the permission was denied, also at the daemon's start, the daemon deletes the token, so the next
dictation shows the dialog again, after a restart of the daemon as well.

## Development

The toolchain comes from [mise](https://mise.jdx.dev/): `rust-toolchain.toml` pins Rust, `mise.toml` the cargo tools and
deno. Building also needs the ALSA development files and `pkg-config` (Debian and Ubuntu: `libasound2-dev pkg-config`).

```sh
mise run setup   # install the toolchain, the cargo tools and deno, enable the repository's git hooks
mise run gate    # most of CI's checks, run locally (the list: mise.toml)
```

The client is one binary crate, its modules grouped by role. `src/session.rs` is the core, the platform-independent
logic of a dictation. `src/audio/` holds the audio source (the microphone and its resampling to 16 kHz mono),
`src/servers/` the clients of the servers (today the final server's, with its WAV encoding), `src/platform/` the
platform parts (today the insertion on KDE Plasma: the clipboard through `ext-data-control`, the remote-desktop portal
and its emulated keyboard, and which clipboard contents are kept and when they come back), and `src/daemon.rs` the
daemon, with the socket protocol it shares with `toggle` in `src/daemon/protocol.rs` and the `toggle` command in
`src/daemon/toggle.rs`. The configuration (`config.rs`), the XDG directories (`xdg.rs`) and the log lines (`log.rs`) sit
at the top.

## License

MIT — see [LICENSE](LICENSE).
