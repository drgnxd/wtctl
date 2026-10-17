# wtctl

`wtctl` manages one detached Git worktree per task for coding agents that work
in parallel. It creates each worktree, records which agent process owns it,
fast-forwards `main` to finished work, and removes the worktree only when doing
so cannot lose anything. It also audits and safely finishes legacy session-owned
worktrees created by the earlier lifecycle.

Ownership is the agent process itself, not a session-id environment variable,
so a stale inherited variable cannot block or misattribute a task.

## Install

```sh
nix build github:drgnxd/wtctl
nix profile install github:drgnxd/wtctl
cargo install --path .                # from a checkout
```

It needs `git` at run time and `lsof` (or `/proc` on Linux) for the process
check. Supported systems: aarch64-darwin, x86_64-linux, aarch64-linux.

## Commands

| Command | Purpose |
|---|---|
| `capabilities [--json]` | Advertise the stable lifecycle feature contract to callers. |
| `create --repo <repo> --slug <slug> [--task <text>] [--from <commit>] [--parent <id>] [--worktrees-root <path>] [--external-writers-quiesced]` | New locked, detached task worktree; Git-annex repositories require the writer acknowledgement. |
| `whoami [--path <p>]` | Map a path inside a task worktree back to its task, owner, and owner state. |
| `list [--mine\|--orphans\|--all\|--wtctl\|--legacy] [--active-hours <n>] [--main <branch>\|--base <ref>] [--json]` | Owner/session activity, integration, cleanliness, and removal status for wtctl and legacy worktrees. |
| `integrate --repo <repo> --id <id> --external-writers-quiesced [--main <branch>] [--allow-annex-filter]` | Fast-forward `main` to the task tip. |
| `finish --repo <repo> (--id <id>\|--legacy-worktree-id <id>\|--expect-branch <branch>) --external-writers-quiesced` | Integrate completed work and remove that exact worktree under one lock; supports `--keep-worktree`, `--integration-message`, filter/signing opt-ins, and artifact cleanup options. |
| `remove --repo <repo> (--id <id>\|--legacy-worktree-id <id>\|--expect-branch <branch>) [--external-writers-quiesced] [--base <ref>]` | Remove one exact wtctl or legacy worktree owned by the caller; actual removal requires writer acknowledgement and supports dry-run, reflog opt-in, and artifact cleanup options. |
| `reap [--dry-run] [--grace-secs <n>] [--external-writers-quiesced] [--main <branch>\|--base <ref>]` | Delete removable worktrees whose owner is dead. |
| `adopt --repo <repo> --id <id>` | Take over a task whose owner is dead. |

Hook-sensitive commands accept `--allow-repository-hooks`. Exit codes: 0 ok,
1 error, 2 usage, 3 `main` diverged, 4 refused by a safety check, 5 lock busy.

Callers supporting multiple installed binary generations should inspect
`wtctl capabilities --json` before relying on `finish`, legacy-session
operations, Git-annex handling, or artifact quarantine. The capability schema
is versioned independently of the package version.

## Workflow

1. `create` makes `$XDG_STATE_HOME/<repo>/worktrees/<task_id>` (falling back
   to `$HOME/.local/state` when `XDG_STATE_HOME` is unset or relative) with
   `git worktree add --detach --lock --reason "agent-task <id>"`. The task id
   is `<slug>-<YYYYMMDD>-<8 hex>`. A parent agent creates the worktree before
   delegating and hands over the path and id.
2. Edit and commit inside the worktree.
3. `finish` integrates the task and removes it while holding the shared lock.
   If `main` advanced, it merges `main` into the owned task worktree without
   rebasing. A conflict leaves the worktree in place for resolution. Use
   `--keep-worktree` to integrate without removing it.

After a crash or resume, `list --orphans` shows tasks whose owner is dead.
Nothing is adopted automatically.

## Ownership model

- **Task marker.** `agent-task.json` (schema 1) in the worktree's Git admin
  directory records the task id, worktree path, base commit, owner, and an
  optional session id kept only as a hint. A marker whose path differs from
  Git's registration is `invalid` and never acted on.
- **Owner.** The nearest ancestor process whose executable is a known agent
  client (`claude`, `opencode`, `copilot`, or `node`/`bun` running one), recorded
  as pid plus start time. With no such ancestor, ownership-dependent commands
  refuse.
- **Liveness** is `alive`, `dead`, or `unknown`. `dead` needs positive
  evidence: the pid is gone, is a zombie, or has a readable but different
  start time. `reap` and `adopt` act only on `dead`; `integrate` and `remove`
  require the caller to be the live owner.
- **Locks.** `integrate`, `remove`, `reap`, and `adopt` take a non-blocking
  `flock` on `<git-common-dir>/agent-locks/integrate.lock`, then an
  mkdir-based `main-integration.lock` (owner file `<token>\n<pid>\n<host>\n`,
  released by renaming to `main-integration.lock.released.<token>`) so older
  tooling using that protocol is excluded too. SIGINT, SIGTERM, and SIGHUP are
  held back from taking the lock until the result or error is written.
  Mutations are appended to `<git-common-dir>/agent-audit.jsonl`. A process
  that dies from a signal may still have completed its change (for example,
  `main` already fast-forwarded); check with `wtctl list` or
  `git rev-parse refs/heads/main` before retrying.
- **Ref updates.** With `main` checked out, `wtctl` re-checks the recorded OID
  and runs `git merge --ff-only --no-overwrite-ignore`; otherwise it uses
  `git update-ref` with the old OID as compare-and-swap.

## Removal safety

A worktree is removable only when all of these hold:

- no rebase, merge, cherry-pick, revert, sequencer, or bisect state, and no
  files under the admin directory's `refs/` or `logs/refs/`;
- no tracked, untracked, or ignored changes, except explicitly selected
  generated artifacts that are signed and quarantined before removal;
- no assume-unchanged or skip-worktree entries;
- no submodule state or gitlink changes (they require a separate task);
- every commit reachable from `HEAD`, its reflog, and per-worktree refs
  (`refs/worktree`, `refs/bisect`, `refs/rewritten`) is in `main`; a missing
  reflog fails closed;
- no process has its cwd inside the worktree (`lsof -d cwd`, or `/proc/*/cwd`
  on Linux). The scan is trusted only if it reports `wtctl`'s own cwd, is
  well formed, and finishes within 30 seconds. Entries that `lsof` escapes
  (non-ASCII or control bytes) are re-read from the kernel; if that fails and
  the entry could name the worktree, removal fails closed;
- the worktree is unlocked or locked with exactly `agent-task <id>`.

`reap` also waits a grace period after creation or adoption (default 600 s),
re-checks each worktree right before removing it, reports anything it keeps as
`orphan<TAB><id><TAB><reason>`, and stops starting removals after an
interrupt. `--dry-run` skips the process check.

Hooks: an executable `reference-transaction` hook always blocks; a
`post-checkout` (create) or `post-merge` (integrate) hook blocks until
`--allow-repository-hooks` is passed, including hooks a relative
`core.hooksPath` would pick up from the source commit.

## Generated artifacts and quarantine

`finish` discards recognized generated artifacts by default: `.pytest_cache`,
`.ruff_cache`, `__pycache__`, root `target`, `rust/target`, the root `result`
symlink, a marked `git-hooks.nix` `.pre-commit-config.yaml`, and a verified
Nix-direnv `.direnv` cache. Unknown `.direnv` layouts are retained. Direct
`remove` keeps generated artifact handling opt-in through
`--discard-generated-caches`, `--discard-generated-build-outputs`,
`--discard-direnv-cache`, or `--discard-generated-artifacts`.

`reap` also selects only disposable Python caches (`.pytest_cache`,
`.ruff_cache`, and `__pycache__`) for cleanup. Other ignored or untracked data
keeps an orphan from being removed.

Before Git removes a worktree, selected paths are moved without replacement to
`$XDG_STATE_HOME/<repo>/worktrees/.removal-quarantine` on the same filesystem.
If removal fails, wtctl restores paths only after rechecking worktree identity,
HEAD, and destination vacancy. `--preserve-generated-artifacts` keeps selected
artifacts in the private quarantine after successful removal. Purge them later
with `remove --repo <repo> --purge-preserved-quarantine <operation-id>`; an
unverified `.direnv` purge additionally requires
`--discard-unverified-direnv`. Purging verifies saved signatures and journals
per-entry progress so an interrupted purge can resume safely.

A root `state.db` symlink is unlinked only when it resolves to the regular
database in the registered `main` worktree. The database target is never
modified; the link is restored if removal fails. `--allow-unreferenced-reflog`
explicitly permits discarding commits held only by the target worktree's HEAD
reflog, but never permits an unintegrated HEAD.

When a finish operation must create an integration merge commit, filters are
refused unless they are verified Git-annex filters and
`--allow-annex-filter` is passed. A configured commit signer requires the
separate `--allow-commit-signing` opt-in. `--integration-message` selects a
repository-conforming merge subject.

## Legacy worktrees and Git-annex

`list --all` includes legacy session-owned detached and branch-backed
worktrees. Claude session JSONL and the read-only OpenCode session database
provide activity timestamps; missing or ambiguous activity remains `UNKNOWN`.
Only an idle, integrated legacy worktree owned by the current session receives
a dry-run `wtctl remove` suggestion. Legacy removal requires an exact worktree
ID or branch name.

For Git-annex repositories, `create`, `integrate`, `finish`, `remove`, and
non-dry-run `reap` require `--external-writers-quiesced`. Removal runs `git annex restage`, checks
the registered worktree identity and cleanliness again, verifies both
directions of the `.git` relationship, and normalizes an annex `.git` symlink
to a gitfile immediately before Git removes the worktree. If restage or removal
fails, the worktree and annex metadata are preserved.

Mutating `integrate`, `remove`, `reap`, and quarantine-purge operations require
`--external-writers-quiesced` after the caller has stopped known non-cooperating
writers. The shared lock excludes cooperating wtctl and legacy invocations
only. When changed paths use filters, integration
refuses arbitrary filters. Verified Git-annex filters need the explicit
`--allow-annex-filter` option on `integrate` or `finish`.

## Limitations

- Ownership is cooperative, not authentication.
- Start times have microsecond (macOS) or clock-tick (Linux) resolution, so a
  pid reused within one tick is not detected.
- Sessions and subagents inside one client process share its owner.
- Agent detection relies on executable names.
- Only the files ref backend is supported; with reftable, `integrate` and
  `remove` are expected to fail closed (not tested).
- The process check sees only processes `lsof` or `/proc` may inspect: other
  users' processes (on macOS as well), non-dumpable processes, and processes
  in other mount namespaces can be missed. The own-process sentinel detects a
  truncated listing, not these gaps.
- `reap` scans processes once before its removals, so a process that enters a
  worktree after the scan is not seen.
- Signals that are not held back (SIGKILL, SIGQUIT, ...), crashes, and aborts
  can leave `main-integration.lock` behind; check its owner pid before
  removing it by hand.
- Legacy Copilot worktrees without a recorded session id remain `UNKNOWN` and
  are never offered for automatic removal. New Copilot tasks use process
  ownership when wtctl can identify the agent process.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for commit message conventions and
development checks. GitHub Actions runs the checks on all supported systems.

## License

MIT
