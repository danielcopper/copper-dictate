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
> `copper-dictate toggle`, and the second `toggle` prints the text your transcription server returned. The live preview,
> the shortcut, insertion at the cursor and the overlay are planned, not features.

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
- **Nothing but the dictation leaves your machine** — no telemetry, no connection but the configured endpoint (proxy
  variables are ignored, redirects are not followed), and the recording is dropped once its text is back

## Planned

- **Live preview** — the text appears while you speak, from a self-hosted realtime WebSocket server
- **A global shortcut, insertion at the cursor and an overlay** — KDE Plasma first, behind interfaces

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
```

The endpoint is the full URL, `http` or `https`. The API key never goes into this file. It is read from the file
`api_key_file` names (a relative path counts from the configuration file's directory), or, without that line, from the
environment variable `COPPER_DICTATE_FINAL_API_KEY`; with neither, or with the variable empty, the request carries no
key. A key file that is empty, or a key with spaces, line breaks or characters outside visible ASCII, stops the daemon
at start.

```sh
copper-dictate daemon   # keep it running; it stops at once, naming the path and the field, if the configuration is incomplete
copper-dictate toggle   # start a recording
copper-dictate toggle   # stop it; prints the final text
```

`toggle` talks to the daemon over `$XDG_RUNTIME_DIR/copper-dictate/daemon.sock`. A recording that reaches `max_seconds`
stops and is transcribed by itself; the next `toggle` prints its text instead of starting a new recording. The same
holds for the text, or the error, of a `toggle` that was interrupted before it arrived. A microphone glitch the audio
stack recovers from (an overrun, a device change) does not end the recording; the `toggle` that prints its text also
warns on stderr how often the microphone glitched. A refused real-time priority loses no audio and is only logged. Any
other microphone error fails the dictation. The final server gets twice the recording's length to answer, at least ten
minutes; a server that takes longer fails the dictation. The daemon opens the microphone at 16 kHz where the device
offers that with a sample format of at least 16 bits, and resamples the device's default configuration otherwise; it
logs the configuration it opened at the start of each recording.

## Development

The toolchain comes from [mise](https://mise.jdx.dev/): `rust-toolchain.toml` pins Rust, `mise.toml` the cargo tools and
deno. Building also needs the ALSA development files and `pkg-config` (Debian and Ubuntu: `libasound2-dev pkg-config`).

```sh
mise run setup   # install the toolchain, the cargo tools and deno, enable the repository's git hooks
mise run gate    # most of CI's checks, run locally (the list: mise.toml)
```

## License

MIT — see [LICENSE](LICENSE).
