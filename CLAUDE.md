# copper-dictate — live dictation for Linux desktops

## What This Is

Self-hosted live dictation for Linux desktops: live preview while you speak, an accurate final text when you stop. Today
the final-text path works: `copper-dictate daemon` records from the default microphone (cpal, so ALSA) on a
`copper-dictate toggle`, and the next `toggle` sends the whole recording to an OpenAI-compatible transcription server
and prints the text. The two talk over a Unix socket in `$XDG_RUNTIME_DIR/copper-dictate/`; the configuration is
`$XDG_CONFIG_HOME/copper-dictate/config.toml`, and the API key comes from `api_key_file` or
`COPPER_DICTATE_FINAL_API_KEY`, never from that file. The session logic in `src/session.rs` is platform-independent,
with the audio source and the final server behind traits. Planned: the live text from a realtime WebSocket server, and
the shortcut, insertion and overlay as platform parts behind interfaces, KDE first.

## Where decisions live

Work starts from an issue: open questions under `## To decide` (one checkbox each, checked off with a pointer like
`→ D1` once answered), decisions under `## Decisions` (appended; a changed one is struck through and re-added with its
date). An epic holds its own; a sub-issue says "See epic #N". A hard-to-reverse, surprising trade-off also gets an ADR
in `docs/adr/`, linked from the issue. Every `## Done when` item becomes a test seen failing first, or is marked
"(device)". The PR body repeats the decisions and becomes the squash commit. The `decisions` CI check enforces the issue
sections and the ADR format ([rules](https://github.com/danielcopper/.github#the-decisions-check)); the rest is for
review.

**Terms** are defined in `GLOSSARY.md`: name things in code, issues and docs by it. **ADRs** in `docs/adr/`: read the
ones a change touches before designing it.

In an issue someone else opened, their text stays as they wrote it; the decisions go into a section appended below it,
opened by `> [!NOTE]` / `> Added by the maintainer.`

## Development

- **Setup** `mise run setup` (the toolchain from `rust-toolchain.toml`, the cargo tools and deno from `mise.toml`, and
  `core.hooksPath .githooks`; building also needs the ALSA development files and pkg-config: `libasound2-dev` and
  `pkg-config` on Debian and Ubuntu) · **Tests** `mise run test` (nextest; unit tests beside the code in `src/`, the
  binary's tests in `tests/`) · **Lint** `mise run lint` (clippy, warnings as errors) · **Coverage** `mise run coverage`
  → `lcov.info`.
- **Layout** by role, in one binary crate: `src/session.rs` is the core; `src/audio/` the audio source; `src/servers/`
  the server clients, the live client's too; `src/daemon.rs` the daemon, with `daemon/protocol.rs` (the socket path and
  the one-JSON-message-per-line framing, for every client of the socket) and `daemon/toggle.rs`; `config.rs`, `xdg.rs`
  and `log.rs` (every log line, which adds the `copper-dictate:` prefix) at the top. `src/platform/` comes with the
  first platform part. A directory's module is the file beside it (`audio.rs` next to `audio/`), never a `mod.rs`. A
  library (`lib.rs`) comes once a second binary or a test under `tests/` needs the modules. A new module goes into the
  list of `every_module_sits_in_the_directory_of_its_role` in `src/main.rs`.
- **Toolchain** `rust-toolchain.toml` is the one source for the Rust version; CI, mise and rustup all read it. The cargo
  tools and deno are pinned in `mise.toml`, and CI installs them from there.
- **Lints** are denied in `Cargo.toml` (`[lints]`), not only in CI: clippy `all` and `pedantic`, and no `unwrap` or
  `expect` outside tests (`clippy.toml`). Fix a lint at its root; an `#[allow]` needs a reason the code cannot show.
- **Dependencies** pass `cargo deny` (`deny.toml`): permissive licences only, no duplicate versions, crates.io only.
- **Release** is release-please: the version in `Cargo.toml` and `Cargo.lock` is its output, never edited by hand.
  `mise run build:release` packs the archive the release attaches, with `scripts/package.sh`.
- **The pre-commit hook** formats the staged Rust files (rustfmt) and the staged Markdown files deno.json covers (deno
  fmt), and stops the commit when a formatter fails; a file it would change that also has unstaged changes is committed
  as staged and named.

## Pipeline

- **Gate**: `mise run gate`. **Review checks**: `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
  **Worktree task**: `mise run worktree-new` (a global mise task from the owner's dotfiles, not in this repository).
- **Agent rules**:
  - On the Steam Deck, gate steps run one at a time: `mise run --jobs 1 gate`. Never start a second run while one is
    going.
  - Scratch files never go in the worktree — `deno fmt --check` reads every Markdown file, and cargo builds every
    top-level `.rs` file in `tests/`, `examples/`, `benches/` and `src/bin/`.
  - Stop only the processes you started, by PID.
