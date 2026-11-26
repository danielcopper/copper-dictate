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

> **Pre-1.0 (v0.x).** Nothing of the above works yet. Today the repository holds the scaffold: a `copper-dictate` binary
> that prints its version, and the CI, quality and release pipeline around it. Everything under "Planned" is a plan, not
> a feature.

---

> [!NOTE]
> **How this is built.** The code is written by an AI coding agent working under my direction. The architecture, the
> design decisions and the review are mine, and changes are smoke-tested on real hardware before they ship.

## Planned

- **A client daemon in Rust** on your desktop
- **Live preview** — the text appears while you speak, from a self-hosted realtime WebSocket server
- **Accurate final text** — when you stop, from a self-hosted OpenAI-compatible transcription server
- **Desktop integration behind interfaces** — KDE Plasma first

## Development

The toolchain comes from [mise](https://mise.jdx.dev/): `rust-toolchain.toml` pins Rust, `mise.toml` the cargo tools.

```sh
mise run setup   # install the toolchain and the cargo tools, enable the repository's git hooks
mise run gate    # every check CI runs but Sonar's quality gate
```

## License

MIT — see [LICENSE](LICENSE).
