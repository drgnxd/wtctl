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

It needs `git` at run time and `lsof` for the process
check. Supported system: aarch64-darwin.

## Commands

| Command | Purpose |
|---|---|
| `capabilities [--json]` | Advertise the stable lifecycle feature contract to callers. |
| `create --repo <repo> --slug <slug> [--task <text>] [--from <commit>] [--parent <id>] [--worktrees-root <path>] [--external-writers-quiesced]` | New locked, detached task worktree; Git-annex repositories require the writer acknowledgement. |
| `whoami [--path <p>]` | Map a path inside a task worktree back to its task, owner, and owner state. |
| `list [--mine\|--orphans\|--all\|--wtctl\|--legacy] [--active-hours <n>] [--main <branch>\|--base <ref>] [--json]` | Owner/session activity, integration, cleanliness, and removal status for wtctl and legacy worktrees. |
| `integrate --repo <repo> --id <id> --external-writers-quiesced [--main <branch>] [--allow-annex-filter]` | Fast-forward `main` to the task tip. |
| `finish --repo <repo> (--id <id>\|--legacy-worktree-id <id>\|--expect-branch <branch>) --external-writers-quiesced` | Integrate completed work and remove that exact worktree under one lock; supports `--keep-worktree`, `--integration-message`, `--recover-failed-merge`, filter/signing opt-ins, and artifact cleanup options. |
| `remove --repo <repo> (--id <id>\|--legacy-worktree-id <id>\|--expect-branch <branch>) [--external-writers-quiesced] [--base <ref>]` | Remove one exact wtctl or legacy worktree owned by the caller; actual removal requires writer acknowledgement and supports dry-run, reflog opt-in, and artifact cleanup options. |
| `reap [--dry-run] [--grace-secs <n>] [--external-writers-quiesced] [--main <branch>\|--base <ref>]` | Delete removable worktrees whose owner is dead. |
| `adopt --repo <repo> --id <id> [--session <id>]` | Take over a task whose owner is dead. |
| `stale --repo <repo> [--older-than-days <n>] [--main <branch>] [--json]` | Dead-owner tasks idle for at least `n` days (default 7), with the evidence for deciding whether to abandon them: commits not patch-equivalent to anything on `main`, uncommitted paths, ignored paths, and whether the task is already on `main`. Read-only. |
| `show --repo <repo> --id <id> [--json]` | Task details: owner, session and resume command, age, changes, unintegrated commits, diffstat, and why it is or is not removable. |
| `describe --repo <repo> --id <id> [--task <text>] [--session <id>]` | Update the task text or recorded session of an owned task. |
| `restore --repo <repo> --id <abandoned-id> --slug <slug>` | Start a new task from an abandoned task tip. |
| `doctor --repo <repo> [--fix-stale-lock --external-writers-quiesced]` | Diagnose stale locks, broken markers, missing worktree paths, and orphaned work. |
| `quarantine --repo <repo>` | List preserved artifact quarantines with age and size. |
| `prune --repo <repo> --older-than-days <n> [--quarantine] [--refs] [--dry-run] --external-writers-quiesced` | Purge old quarantines and delete old abandoned or legacy superseded task tips. |
| `--version`, `help <command>`, `<command> --help` | Version and per-command usage. |

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
   `--keep-worktree` to integrate without removing it. Default cleanup follows
   Git's worktree-removal boundary: tracked changes and non-ignored untracked
   files block, while ignored files are removed with the worktree.

If a rejected merge commit leaves `MERGE_HEAD` in the task worktree, `finish`
refuses by default. With a wtctl task `--id` and explicit
`--integration-message`, `--recover-failed-merge` permits aborting the current
merge only after verifying its task/main relationship, expected merge
tree, index and tracked files, autostash state, open worktree users, and any
untracked/ignored paths that could collide with files restored from the task
tip. Recovery requires an explicit `--integration-message`; after a verified
abort, `finish` retries the normal merge with current `main`. A pre-abort check
failure leaves `MERGE_HEAD`, index, tracked worktree, and untracked/ignored paths
unchanged; a failed post-abort check reports the state reached and does not
promise rollback. Annex clean-filter evaluation may have its normal annex-side
effects. With `--allow-annex-filter`, verified Git-annex clean filtering may be
used only when raw file hashing does not match and both the current and
expected-tree attributes resolve to `filter=annex`; other filters are never
run by recovery. Before finish, integration, or removal inspects status or
restages an annex worktree, it checks tracked-path attributes and the effective
task worktree filter configuration, including per-worktree config overrides.

After a crash or resume, `list --orphans` shows tasks whose owner is dead.
Nothing is adopted automatically.

## Inspecting and recovering tasks

- `create --session <id>` records the client session (otherwise
  `CLAUDE_CODE_SESSION_ID` or `OPENCODE_SESSION_ID` is used when set).
  `whoami`, `show`, and `list --json` report it, and `whoami` and `show`
  print the matching resume command for known clients
  (`claude --resume <id>`, `opencode --session <id>`).
- `remove --id <id> --abandon` discards unintegrated work without losing it:
  the task tip is kept under `refs/wtctl/abandoned/<id>` (it only ever moves
  forward to a descendant on a retry),
  then every normal removal check runs against that tip, so a dirty worktree or
  reflog-only commits still refuse. `list --abandoned` shows what is kept and
  `restore` starts a new task from it.
- `--abandon --include-uncommitted` also keeps uncommitted work. Tracked
  changes and untracked, non-ignored files are committed on top of the task tip
  in a private index (the worktree and its real index are not touched), the ref
  points at that snapshot, and only then is the worktree reset so the normal
  removal checks see a clean tree. It refuses if the worktree changed while the
  snapshot was taken. Ignored files are not kept; `--dry-run` lists them as
  `ignored_paths_not_kept`, and a normal removal deletes them with the worktree.
- `--abandon --dead-owner` allows the owner to be dead. The owner must be
  verifiably dead; a live or unverifiable owner still refuses. Because the work
  is preserved in full, this does not need `adopt` first. Pass it only when the
  caller has been told to discard that specific task.
- `prune --refs` also handles legacy `refs/wtctl/superseded/` tips created by
  earlier versions. Current versions no longer create these refs.
- `finish --dry-run` takes no lock and changes nothing. It reports how the task
  would integrate (already integrated, fast-forward, or merge), runs the
  checks a real run would (owner, signing, hooks, filters, merge conflicts,
  worktree state, open processes, artifact cleanup), and lists the flags the
  real run needs. It exits 4 unless the real run would pass with the flags
  given.
- `create --fetch [--remote <name>]` fetches before resolving `--from`, and
  `finish --push [--remote <name>]` pushes the integrated `main` without
  forcing (`pre-push` hooks need `--allow-repository-hooks`). A rejected push is
  reported after the integration, which is already complete.
- `list --all-repos` lists tasks from every repository under the default state
  directory; worktrees created with a custom `--worktrees-root` are not found.

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
- no tracked changes or non-ignored untracked files;
- ignored files are not treated as dirty by the default cleanup and are deleted
  with the worktree, as with ordinary `git worktree remove`;
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

## Optional artifact handling and quarantine

Default `finish` and `remove` follow Git's worktree-removal boundary; ignored
paths are removed with the worktree, and visible untracked files block removal.
No repository configuration file is needed or supported for artifact handling.
Move task results that need to survive outside the worktree or commit them before
finishing. Do not rely on `.gitignore` as a preservation mechanism.

The following opt-in artifact selectors add signature checks and quarantine
handling beyond Git's normal worktree removal. The built-in artifact roots live
in `src/artifact_defaults.json`: `.pytest_cache`,
`.ruff_cache`, `__pycache__`, root `target`, `rust/target`, the root `result`
symlink, and the marked `git-hooks.nix` `.pre-commit-config.yaml`. A verified
Nix-direnv `.direnv` cache has separate validation, including stale caches whose
correctly formed direct `/nix/store` links have been garbage-collected. The full
supported layout must still match; unknown `.direnv` layouts are retained. Direct
`remove` keeps generated artifact handling opt-in through
`--discard-generated-caches`, `--discard-generated-build-outputs`,
`--discard-direnv-cache`, or `--discard-generated-artifacts`.

Repository-specific generated paths are not classified or authorized by
repository metadata, including ignored symlinks that point into the main
worktree. Built-in artifact selectors remain opt-in and are limited to broadly
disposable roots; `.gitignore` patterns do not grant destructive authorization.

For a user-authorized one-off exception, `finish` and `remove` accept
`--discard-artifact-path <relative-file>`. It selects only one ignored or
untracked regular file, requires every path component to be a real directory,
and refuses if the parent contains another tracked, untracked, or ignored file.
It cannot be combined with broad discard selectors. A nested path requires
`finish --keep-worktree` so its parent remains; a worktree-removing `finish` or
`remove` refuses nested selections. With `finish --keep-worktree`, the selected
file is signed and quarantined while the worktree and parent remain. Dry-run
reports the exact path without writing. A failure before quarantine restores the
file only after verifying its signature; a purge failure keeps it in quarantine
and reports the operation ID. Resume with the same worktree and exact path:
`finish --repo <repo> --id <worktree-id> --keep-worktree
--discard-artifact-path <relative-file> --resume-one-off-quarantine <operation-id>
--external-writers-quiesced`. Resume requires the selected path to remain absent
and its parent to remain empty; it purges only the signed quarantined file and
keeps the parent and worktree. This exception is per invocation and does not
change the repository or built-in artifact policy.

`reap` also selects only disposable Python caches (`.pytest_cache`,
`.ruff_cache`, and `__pycache__`) for cleanup. Other ignored or untracked data
keeps an orphan from being removed.

In explicit artifact mode, before Git removes a worktree, selected paths are moved without replacement to
`$XDG_STATE_HOME/<repo>/worktrees/.removal-quarantine` on the same filesystem.
If removal fails, wtctl restores paths only after rechecking worktree identity,
HEAD, and destination vacancy. `--preserve-generated-artifacts` keeps selected
artifacts in the private quarantine after successful removal. Purge them later
with `remove --repo <repo> --purge-preserved-quarantine <operation-id>`; an
unverified `.direnv` purge additionally requires
`--discard-unverified-direnv`. Purging verifies saved signatures and journals
per-entry progress so an interrupted purge can resume safely.

`--allow-unreferenced-reflog` explicitly permits discarding commits held only
by the target worktree's HEAD reflog, but never permits an unintegrated HEAD.

When a finish operation must create an integration merge commit, filters are
refused unless they are verified Git-annex filters and
`--allow-annex-filter` is passed. A configured commit signer requires the
separate `--allow-commit-signing` opt-in. `--integration-message` supplies the
complete repository-conforming commit message; embedded newlines can separate
the subject and body.

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
development checks. GitHub Actions runs the checks on aarch64-darwin.

## License

MIT
