# wtctl

`wtctl` manages one detached Git worktree per task for coding agents that work
in parallel. It creates each worktree, records which agent process owns it,
fast-forwards `main` to finished work, and removes the worktree only when doing
so cannot lose anything.

Ownership is the agent process itself, not a session-id environment variable,
so a stale inherited variable cannot block or misattribute a task.

## Install

```sh
nix build github:<owner>/wtctl        # or: nix profile install ...
cargo install --path .                # from a checkout
```

It needs `git` at run time and `lsof` (or `/proc` on Linux) for the process
check. Supported systems: aarch64-darwin, x86_64-linux, aarch64-linux.

## Commands

| Command | Purpose |
|---|---|
| `create --repo <repo> --slug <slug> [--task <text>] [--from <commit>]` | New locked, detached task worktree; prints `task_id` and `worktree`. |
| `whoami [--path <p>]` | Map a path inside a task worktree back to its task, owner, and owner state. |
| `list [--mine\|--orphans\|--all] [--main <branch>] [--json]` | Owner state, integration, cleanliness, removability, and lock state. |
| `integrate --repo <repo> --id <id> [--main <branch>]` | Fast-forward `main` to the task tip. |
| `remove --repo <repo> --id <id>` | Delete a task worktree the caller owns once it is removable. |
| `reap [--dry-run] [--grace-secs <n>]` | Delete removable worktrees whose owner is dead. |
| `adopt --repo <repo> --id <id>` | Take over a task whose owner is dead. |

Hook-sensitive commands accept `--allow-repository-hooks`. Exit codes: 0 ok,
1 error, 2 usage, 3 `main` diverged, 4 refused by a safety check, 5 lock busy.

## Workflow

1. `create` makes `$XDG_STATE_HOME/<repo>/worktrees/<task_id>` (falling back
   to `$HOME/.local/state` when `XDG_STATE_HOME` is unset or relative) with
   `git worktree add --detach --lock --reason "agent-task <id>"`. The task id
   is `<slug>-<YYYYMMDD>-<8 hex>`. A parent agent creates the worktree before
   delegating and hands over the path and id.
2. Edit and commit inside the worktree.
3. `integrate` fast-forwards `main`. Exit 3 means `main` advanced; it prints
   `conflict<TAB><path>` lines from `git merge-tree`. Merge `main` into the
   task worktree, commit, and retry. Do not rebase: rewritten commits stay in
   the reflog and keep the worktree from being removed.
4. From outside the worktree, `remove` it.

After a crash or resume, `list --orphans` shows tasks whose owner is dead.
Nothing is adopted automatically.

## Ownership model

- **Task marker.** `agent-task.json` (schema 1) in the worktree's Git admin
  directory records the task id, worktree path, base commit, owner, and an
  optional session id kept only as a hint. A marker whose path differs from
  Git's registration is `invalid` and never acted on.
- **Owner.** The nearest ancestor process whose executable is a known agent
  client (`claude`, `opencode`, or `node`/`bun` running one), recorded as pid
  plus start time. With no such ancestor, ownership-dependent commands refuse.
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
  Mutations are appended to `<git-common-dir>/agent-audit.jsonl`.
- **Ref updates.** With `main` checked out, `wtctl` re-checks the recorded OID
  and runs `git merge --ff-only --no-overwrite-ignore`; otherwise it uses
  `git update-ref` with the old OID as compare-and-swap.

## Removal safety

A worktree is removable only when all of these hold:

- no rebase, merge, cherry-pick, revert, sequencer, or bisect state, and no
  files under the admin directory's `refs/` or `logs/refs/`;
- no tracked or untracked changes, no ignored paths other than `__pycache__`,
  `.pytest_cache`, and `.ruff_cache`, and no assume-unchanged or
  skip-worktree entries;
- every commit reachable from `HEAD`, its reflog, and per-worktree refs
  (`refs/worktree`, `refs/bisect`, `refs/rewritten`) is in `main`; a missing
  reflog fails closed;
- no process has its cwd inside the worktree (`lsof -d cwd`, or `/proc/*/cwd`
  on Linux). The scan is trusted only if it reports `wtctl`'s own cwd and is
  well formed; escaped paths that could name the worktree fail closed;
- the worktree is unlocked or locked with exactly `agent-task <id>`.

`reap` also waits a grace period after creation or adoption (default 600 s),
re-checks each worktree right before removing it, reports anything it keeps as
`orphan<TAB><id><TAB><reason>`, and stops starting removals after an
interrupt. `--dry-run` skips the process check.

Hooks: an executable `reference-transaction` hook always blocks; a
`post-checkout` (create) or `post-merge` (integrate) hook blocks until
`--allow-repository-hooks` is passed, including hooks a relative
`core.hooksPath` would pick up from the source commit.

## Limitations

- Ownership is cooperative, not authentication.
- Start times have microsecond (macOS) or clock-tick (Linux) resolution, so a
  pid reused within one tick is not detected.
- Sessions and subagents inside one client process share its owner.
- Agent detection relies on executable names.
- Only the files ref backend is supported; with reftable, `integrate` and
  `remove` are expected to fail closed (not tested).
- The process check sees only processes `lsof` or `/proc` may inspect, and not
  other mount namespaces.
- Signals that are not held back (SIGKILL, SIGQUIT, ...), crashes, and aborts
  can leave `main-integration.lock` behind; check its owner pid before
  removing it by hand.
- git-annex worktrees are untested.

## License

MIT
