# Contributing

## Commit messages

Use an English Conventional Commits subject:

```text
<type>[optional scope][!]: <summary>
```

Use a lowercase type such as `feat`, `fix`, `docs`, `build`, `ci`, `test`, or
`chore`. Keep the summary specific and concise. Use `!` before the colon for a
breaking change and describe it in the body or a `BREAKING CHANGE:` footer.
Separate an optional body from the subject with a blank line.

CI checks the subject syntax for every non-merge commit in the pull request or
push. Merge commits are exempt because GitHub may generate their subjects. CI
cannot determine whether prose is English or whether a summary accurately
describes the change; contributors and reviewers remain responsible for that.

## Development checks

Enter the Nix development shell, then run:

```sh
nix develop
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features test-agent-names -- -D warnings
cargo test
cargo test --features test-agent-names
nix flake check
nix build .#wtctl --no-link
```

The GitHub Actions workflow runs these checks on `aarch64-darwin`,
`x86_64-linux`, and `aarch64-linux`.
