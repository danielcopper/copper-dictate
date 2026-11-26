# copper-dictate — live dictation for Linux desktops

## What This Is

Self-hosted live dictation for Linux desktops: live preview while you speak, an accurate final text when you stop. Today
the repository holds only the scaffold — one crate whose binary prints its version for `--version`, and the CI, Sonar
and release pipeline around it. Planned: a client daemon in Rust that takes the live text from a realtime WebSocket
server and the final text from an OpenAI-compatible transcription server, with the platform parts behind interfaces, KDE
first.

## Where decisions live

Work starts from an issue: open questions under `## To decide` (one checkbox each, checked off with a pointer like
`→ D1` once answered), decisions under `## Decisions` (appended; a changed one is struck through and re-added with its
date). An epic holds its own; a sub-issue says "See epic #N". A hard-to-reverse, surprising trade-off also gets an ADR
in `docs/adr/`, linked from the issue. Every `## Done when` item becomes a test seen failing first, or is marked
"(device)". The PR body repeats the decisions and becomes the squash commit. The `decisions` CI check enforces the issue
sections and the ADR format ([rules](https://github.com/danielcopper/.github#the-decisions-check)); the rest is for
review.

In an issue someone else opened, their text stays as they wrote it; the decisions go into a section appended below it,
opened by `> [!NOTE]` / `> Added by the maintainer.`

## Development

- **Setup** `mise run setup` (the toolchain from `rust-toolchain.toml`, the cargo tools and deno from `mise.toml`, and
  `core.hooksPath .githooks`) · **Tests** `mise run test` (nextest; unit tests beside the code in `src/`, the binary's
  tests in `tests/`) · **Lint** `mise run lint` (clippy, warnings as errors) · **Coverage** `mise run coverage` →
  `lcov.info` · **Gate** `mise run gate` — every job in ci.yml but the Sonar scan.
- **Toolchain** `rust-toolchain.toml` is the one source for the Rust version; CI, mise and rustup all read it. The cargo
  tools and deno are pinned in `mise.toml`, and CI installs them from there.
- **Lints** are denied in `Cargo.toml` (`[lints]`), not only in CI: clippy `all` and `pedantic`, and no `unwrap` or
  `expect` outside tests (`clippy.toml`). Fix a lint at its root; an `#[allow]` needs a reason the code cannot show.
- **Dependencies** pass `cargo deny` (`deny.toml`): permissive licences only, no duplicate versions, crates.io only. A
  licence allowed there is also accepted in `about.toml`, from which `cargo about` writes the third-party notices the
  release archive carries.
- **Release** is release-please: the version in `Cargo.toml` and `Cargo.lock` is its output, never edited by hand.
  `mise run build:release` packs the archive the release attaches, with `scripts/package.sh`.
- **The pre-commit hook** formats what is staged of each Rust (rustfmt) and Markdown (deno fmt) file and stops the
  commit when a formatter fails; a file that also has unstaged changes keeps its staged content and is named.

## Pipeline

- **Gate**: `mise run gate`. **Review checks**: `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.
  **Worktree task**: `mise run worktree-new` (a global mise task from the owner's dotfiles, not in this repository).
- **Agent rules**:
  - On the Steam Deck, gate steps run one at a time: `mise run --jobs 1 gate`. Never start a second run while one is
    going.
  - Scratch files never go in the worktree — `deno fmt --check` reads every Markdown file, and cargo builds any `.rs`
    file under `tests/`, `examples/`, `benches/` or `src/bin/`.
  - Stop only the processes you started, by PID.
