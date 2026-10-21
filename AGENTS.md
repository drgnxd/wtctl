# Repository Rules

`wtctl` is a single Rust binary that manages detached task worktrees. Keep it
small, standard-library first, and fail closed.

## Build and test

- Keep Cargo output outside the checkout, for example
  `CARGO_TARGET_DIR="$XDG_CACHE_HOME/wtctl-target"`; a `target/` inside a task
  worktree is ignored data that keeps the worktree from being removed.
- `nix develop` provides cargo, rustc, clippy, rustfmt, git, and lsof.
- Before committing, run:
  - `cargo fmt --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo clippy --all-targets --features test-agent-names -- -D warnings`
  - `cargo test` and `cargo test --features test-agent-names`
  - `nix flake check` and `nix build`
- The CLI tests in `tests/cli.rs` need the `test-agent-names` feature, which
  lets `WTCTL_AGENT_NAMES` name the agent process. Never enable it in release
  builds or read that variable outside the feature gate.

## Conventions

- Commits follow the Conventional Commits subject format in English; see
  `CONTRIBUTING.md`. CI checks subject syntax.
- This repository is public. Keep documentation, examples, and test fixtures
  consumer-neutral: use synthetic values and generic filenames; never copy a
  consumer repository's paths, filenames, labels, or data.
- Comments explain why, never what.
