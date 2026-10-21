//! wtctl: create, inspect, integrate, and clean up detached task worktrees.
//!
//! New tasks are process-owned; legacy worktrees retain session-based ownership.
//! Mutating commands serialize on a flock and the legacy mkdir lock.

mod artifact;
mod git;
mod legacy;
mod lock;
mod preflight;
mod procinfo;
mod task;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use procinfo::{Liveness, Owner};
use serde_json::{json, Value};
use task::{canon, owner_json, Entry, Marker, Meta};

const DEFAULT_GRACE_SECS: i64 = 600;

const USAGE: &str = "usage: wtctl <command> [options]

  capabilities [--json]       Report lifecycle capabilities for caller gating
  create  --repo <path> --slug <slug> [--task <text>] [--session <id>] [--from <commit>] [--parent <id>] [--worktrees-root <path>] [--allow-repository-hooks] [--external-writers-quiesced] [--json]
  whoami  [--path <path>] [--json]
  list    [--repo <path>] [--mine | --orphans | --all | --wtctl | --legacy | --abandoned] [--active-hours <n>] [--main <branch> | --base <ref>] [--json]
  integrate --repo <path> --id <task-id> [--main <branch>] [--allow-repository-hooks] [--allow-annex-filter] [--external-writers-quiesced]
  finish  --repo <path> --id <task-id> [--main <branch>] [--integration-message <text>] [--recover-failed-merge] [--allow-repository-hooks] [--allow-annex-filter] [--allow-commit-signing] [--squash | --rebase] [--keep-worktree] [--dry-run] [--json] [artifact cleanup options] --external-writers-quiesced
  reap    [--repo <path>] [--dry-run] [--grace-secs <n>] [--main <branch> | --base <ref>] [--external-writers-quiesced]
  adopt   --repo <path> --id <task-id> [--session <id>]
  show    --repo <path> --id <task-id> [--main <branch> | --base <ref>] [--json]
  describe --repo <path> --id <task-id> [--task <text>] [--session <id>] [--json]
  quarantine --repo <path> [--json]
  prune   --repo <path> --older-than-days <n> [--quarantine] [--refs] [--dry-run] [--main <branch>] [--json] [--external-writers-quiesced]
  doctor  --repo <path> [--fix-stale-lock --external-writers-quiesced] [--json]
  restore --repo <path> --id <abandoned-task-id> --slug <slug> [--task <text>] [--session <id>] [--worktrees-root <path>] [--allow-repository-hooks] [--external-writers-quiesced] [--json]
  remove  --repo <path> (--id <task-id> | --legacy-worktree-id <id> | --expect-branch <branch>) [--abandon] [--delete-branch] [--dry-run] [artifact cleanup options] [--main <branch> | --base <ref>] [--external-writers-quiesced]

  artifact cleanup options: --discard-direnv-cache, --discard-generated-caches,
    --discard-generated-build-outputs, --discard-generated-artifacts,
    --preserve-generated-artifacts, --purge-preserved-quarantine <operation-id>

exit codes: 0 ok, 1 error, 2 usage, 3 main diverged (rebase needed),
4 refused by a safety check, 5 lock busy";

macro_rules! out {
    ($($arg:tt)*) => {{
        // A closed stdout (e.g. `| head`) must not abort a mutation midway.
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// The usage block for one command: its line plus indented continuation lines.
fn command_help(name: &str) -> R<()> {
    let mut block: Vec<&str> = Vec::new();
    for line in USAGE.lines() {
        let command_line = line.starts_with("  ") && !line.starts_with("   ");
        if block.is_empty() {
            if command_line && line.split_whitespace().next() == Some(name) {
                block.push(line);
            }
        } else if line.starts_with("    ") && !command_line {
            block.push(line);
        } else {
            break;
        }
    }
    let Some((first, rest)) = block.split_first() else {
        return Err(Fail::new(2, format!("unknown command {name:?}\n{USAGE}")));
    };
    out!(
        "usage: wtctl {}",
        first.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    for line in rest {
        out!("{line}");
    }
    if matches!(name, "finish" | "remove") {
        let options = USAGE
            .lines()
            .skip_while(|line| !line.starts_with("  artifact cleanup options"))
            .take_while(|line| !line.is_empty());
        out!("");
        for line in options {
            out!("{line}");
        }
    }
    Ok(())
}

mod abandon;
mod doctor;
mod finish_plan;
mod housekeeping;
mod show;
mod strategy;

#[derive(Debug)]
pub struct Fail {
    pub code: i32,
    pub msg: String,
}

impl Fail {
    pub fn new(code: i32, msg: impl Into<String>) -> Self {
        Fail {
            code,
            msg: msg.into(),
        }
    }
}

type R<T> = Result<T, Fail>;

fn refuse<T>(msg: impl Into<String>) -> R<T> {
    Err(Fail::new(4, msg))
}

fn cmd_capabilities(a: &Args) -> R<()> {
    if a.values.keys().any(|key| key != "--json") || a.flags.iter().any(|flag| flag != "--json") {
        return Err(Fail::new(2, "capabilities accepts only --json"));
    }
    let capabilities = json!({
        "schema": 1,
        "create": true,
        "finish": true,
        "legacy_session_lifecycle": true,
        "git_annex": true,
        "artifact_quarantine": true,
        "quarantine_purge": true,
    });
    if a.flag("--json") {
        out!("{capabilities}");
    } else {
        out!("wtctl lifecycle capabilities: schema 1, legacy, git-annex, artifact-quarantine");
    }
    Ok(())
}

// ---------------------------------------------------------------- arguments

struct Args {
    values: HashMap<String, OsString>,
    flags: Vec<String>,
}

const VALUE_OPTS: &[&str] = &[
    "--repo",
    "--slug",
    "--task",
    "--from",
    "--id",
    "--path",
    "--main",
    "--grace-secs",
    "--active-hours",
    "--legacy-worktree-id",
    "--expect-branch",
    "--purge-preserved-quarantine",
    "--integration-message",
    "--parent",
    "--worktrees-root",
    "--base",
    "--lock-name",
    "--session",
    "--older-than-days",
];
const FLAG_OPTS: &[&str] = &[
    "--mine",
    "--orphans",
    "--all",
    "--wtctl",
    "--legacy",
    "--dry-run",
    "--json",
    "--allow-repository-hooks",
    "--external-writers-quiesced",
    "--delete-branch",
    "--discard-direnv-cache",
    "--discard-generated-caches",
    "--discard-generated-build-outputs",
    "--discard-generated-artifacts",
    "--preserve-generated-artifacts",
    "--discard-unverified-direnv",
    "--allow-unreferenced-reflog",
    "--keep-worktree",
    "--abandon",
    "--squash",
    "--quarantine",
    "--refs",
    "--fix-stale-lock",
    "--rebase",
    "--abandoned",
    "--allow-annex-filter",
    "--allow-commit-signing",
    "--recover-failed-merge",
];

impl Args {
    fn parse(raw: &[OsString]) -> R<Self> {
        let mut values = HashMap::new();
        let mut flags = Vec::new();
        let mut it = raw.iter();
        while let Some(arg) = it.next() {
            let bytes = arg.as_bytes();
            let (name, inline) = match bytes.iter().position(|b| *b == b'=') {
                Some(i) if bytes.starts_with(b"--") => (
                    &bytes[..i],
                    Some(OsStr::from_bytes(&bytes[i + 1..]).to_os_string()),
                ),
                _ => (bytes, None),
            };
            let name = std::str::from_utf8(name)
                .map_err(|_| Fail::new(2, format!("unknown argument {arg:?}\n{USAGE}")))?;
            if VALUE_OPTS.contains(&name) {
                let value = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .cloned()
                        .ok_or_else(|| Fail::new(2, format!("{name} needs a value")))?,
                };
                values.insert(name.to_string(), value);
            } else if FLAG_OPTS.contains(&name) && inline.is_none() {
                flags.push(name.to_string());
            } else {
                return Err(Fail::new(2, format!("unknown argument {arg:?}\n{USAGE}")));
            }
        }
        Ok(Args { values, flags })
    }

    fn os(&self, name: &str) -> Option<&OsStr> {
        self.values.get(name).map(OsString::as_os_str)
    }

    fn get(&self, name: &str) -> R<Option<&str>> {
        self.os(name)
            .map(|v| {
                v.to_str()
                    .ok_or_else(|| Fail::new(2, format!("{name} must be valid UTF-8")))
            })
            .transpose()
    }

    fn need(&self, name: &str) -> R<&str> {
        self.get(name)?
            .ok_or_else(|| Fail::new(2, format!("{name} is required\n{USAGE}")))
    }

    fn need_os(&self, name: &str) -> R<&OsStr> {
        self.os(name)
            .ok_or_else(|| Fail::new(2, format!("{name} is required\n{USAGE}")))
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn repo(&self, required: bool) -> R<Repo> {
        let path = if required {
            self.need_os("--repo")?
        } else {
            self.os("--repo").unwrap_or(OsStr::new("."))
        };
        Repo::open(Path::new(path))
    }
}

fn artifact_options(a: &Args, finish_default: bool) -> R<artifact::Options> {
    let preserve = a.flag("--preserve-generated-artifacts");
    let discard_all = a.flag("--discard-generated-artifacts") || (finish_default && !preserve);
    let discard_caches = a.flag("--discard-generated-caches");
    if discard_all && preserve {
        return Err(Fail::new(
            2,
            "--discard-generated-artifacts cannot be combined with --preserve-generated-artifacts",
        ));
    }
    if discard_caches && preserve {
        return Err(Fail::new(
            2,
            "--discard-generated-caches cannot be combined with --preserve-generated-artifacts",
        ));
    }
    Ok(artifact::Options {
        discard_direnv: a.flag("--discard-direnv-cache"),
        discard_caches,
        discard_build_outputs: a.flag("--discard-generated-build-outputs"),
        discard_all,
        preserve_generated: preserve,
        allow_unreferenced_reflog: a.flag("--allow-unreferenced-reflog"),
    })
}

/// `refs/heads/<branch>` for `--main`, validated by Git's own branch-name rules.
fn main_ref(a: &Args, cwd: &Path) -> R<(String, String)> {
    let Some(branch) = a.get("--main")? else {
        return Ok(("main".into(), "refs/heads/main".into()));
    };
    // `@{-N}` and `@` name branches relative to some checkout's history.
    if branch.starts_with('-') || branch.contains("@{") || branch == "@" {
        return Err(Fail::new(
            2,
            format!("--main {branch:?} is not a branch name"),
        ));
    }
    let out = git::output(cwd, ["check-ref-format", "--branch", branch])?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || name.is_empty() {
        return Err(Fail::new(
            2,
            format!("--main {branch:?} is not a branch name"),
        ));
    }
    let refname = format!("refs/heads/{name}");
    Ok((name, refname))
}

fn base_ref(a: &Args, cwd: &Path) -> R<(String, String)> {
    let Some(base) = a.get("--base")? else {
        return main_ref(a, cwd);
    };
    if a.get("--main")?.is_some() {
        return Err(Fail::new(2, "choose one of --main and --base"));
    }
    if base.starts_with('-') {
        return Err(Fail::new(2, "--base must be a commit ref or OID"));
    }
    let expression = format!("{base}^{{commit}}");
    let resolved = git::output(
        cwd,
        ["rev-parse", "--verify", "--end-of-options", &expression],
    )?;
    let oid = String::from_utf8_lossy(&resolved.stdout).trim().to_string();
    if !resolved.status.success() || !git::is_oid(&oid) {
        return Err(Fail::new(
            2,
            format!("--base {base:?} does not resolve to a commit"),
        ));
    }
    Ok((base.to_string(), oid))
}

// -------------------------------------------------------------- environment

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn local_date() -> String {
    let t = now() as libc::time_t;
    // SAFETY: an all-zero tm is a valid value for localtime_r to overwrite.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference valid, initialized values.
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

fn random_hex(bytes: usize) -> R<String> {
    let mut buf = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| Fail::new(1, format!("cannot read /dev/urandom: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn state_home() -> R<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_STATE_HOME") {
        if Path::new(&v).is_absolute() {
            return Ok(PathBuf::from(v));
        }
    }
    match std::env::var_os("HOME") {
        Some(h) if Path::new(&h).is_absolute() => Ok(PathBuf::from(h).join(".local/state")),
        _ => Err(Fail::new(
            1,
            "HOME is unset or not absolute and XDG_STATE_HOME is not absolute",
        )),
    }
}

fn me() -> R<Owner> {
    procinfo::find_agent().ok_or_else(|| {
        Fail::new(
            4,
            "no coding-agent ancestor process (claude/opencode/copilot) found; \
             run wtctl from the agent's own shell",
        )
    })
}

fn is_mine(marker: &Marker, me: Option<&Owner>) -> bool {
    me.is_some_and(|me| me.pid == marker.owner.pid && me.start == marker.owner.start)
        && procinfo::liveness(&marker.owner) == Liveness::Alive
}

// ---------------------------------------------------------------- repository

struct Repo {
    root: PathBuf,
    common: PathBuf,
}

impl Repo {
    fn open(path: &Path) -> R<Repo> {
        let root = canon(path);
        let out = git::output(
            &root,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        if !out.status.success() {
            return Err(Fail::new(
                1,
                format!("{} is not a Git repository", root.display()),
            ));
        }
        let common = OsStr::from_bytes(out.stdout.trim_ascii_end());
        Ok(Repo {
            root,
            common: canon(Path::new(common)),
        })
    }

    fn name(&self) -> String {
        let main = if self.common.file_name() == Some(".git".as_ref()) {
            self.common.parent().unwrap_or(&self.common)
        } else {
            &self.common
        };
        main.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into())
    }

    fn lock(&self) -> R<lock::RepoLock> {
        lock::acquire(&self.common, random_hex(16)?)
    }

    fn uses_annex(&self) -> R<bool> {
        let filter = git::output(&self.root, ["config", "--get", "filter.annex.process"])?;
        Ok(
            (filter.status.success() && !filter.stdout.trim_ascii().is_empty())
                || self.common.join("annex").exists(),
        )
    }

    fn require_annex_ready(&self, external_writers_quiesced: bool) -> R<bool> {
        if !self.uses_annex()? {
            return Ok(false);
        }
        if !external_writers_quiesced {
            return refuse("this repository uses git-annex; coordinate external writers and pass --external-writers-quiesced");
        }
        let version = git::output(&self.root, ["annex", "version"])?;
        if !version.status.success() {
            return refuse(format!(
                "this repository uses git-annex but `git annex version` failed: {}",
                git::stderr_of(&version)
            ));
        }
        Ok(true)
    }

    fn audit(&self, record: Value) {
        let line = format!("{record}\n");
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.common.join("agent-audit.jsonl"))
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }

    fn find_task(&self, id: &str) -> R<(Entry, Marker)> {
        if !task::valid_task_id(id) {
            return Err(Fail::new(2, format!("{id:?} is not a wtctl task id")));
        }
        let direct = Entry::load(self.common.join("worktrees").join(id));
        let found = direct
            .into_iter()
            .chain(task::entries(&self.common))
            .find_map(|e| {
                let m = e.marker().filter(|m| m.task_id == id)?.clone();
                Some((e, m))
            });
        found.ok_or_else(|| {
            Fail::new(
                1,
                format!("no valid wtctl task {id} (see `wtctl list --all`)"),
            )
        })
    }
}

// --------------------------------------------------------------------- hooks

/// Hook directory as Git resolves it from `cwd`, plus hooks a relative
/// `core.hooksPath` would pick up from `source` once checked out.
fn active_hooks(
    cwd: &Path,
    names: &[&str],
    source: Option<&str>,
    hooks_dir: Option<&Path>,
) -> R<Vec<String>> {
    let dir = match hooks_dir {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from(
            git::run(
                cwd,
                ["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
            )?
            .trim_end(),
        ),
    };
    let mut found = git::executable_hooks(&dir, names);
    let configured = git::output(cwd, ["config", "--get", "core.hooksPath"])?;
    let configured = String::from_utf8_lossy(&configured.stdout)
        .trim()
        .to_string();
    if configured.is_empty() || Path::new(&configured).is_absolute() || configured.starts_with('~')
    {
        return Ok(found);
    }
    let Some(source) = source else {
        return Ok(found);
    };
    if Path::new(&configured)
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return refuse(
            "a relative core.hooksPath containing '..' cannot be inspected; use an absolute path",
        );
    }
    let prefix = configured.trim_end_matches('/');
    let mut args = vec![
        "ls-tree".to_string(),
        "-z".into(),
        source.to_string(),
        "--".into(),
    ];
    args.extend(names.iter().map(|n| format!("{prefix}/{n}")));
    for record in git::run(cwd, &args)?.split('\0') {
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mode = meta.split(' ').next().unwrap_or("");
        let name = path.rsplit('/').next().unwrap_or("");
        if matches!(mode, "100755" | "120000") && !found.iter().any(|f| f == name) {
            found.push(name.to_string());
        }
    }
    Ok(found)
}

fn check_hooks(found: &[String], allow: bool) -> R<()> {
    if found.iter().any(|h| h == "reference-transaction") {
        return refuse("a reference-transaction hook is installed; wtctl never runs it");
    }
    if !found.is_empty() && !allow {
        return refuse(format!(
            "hook(s) {} would run; inspect them, then pass --allow-repository-hooks",
            found.join(", ")
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------- commands

fn relock(repo: &Repo, path: &OsStr, id: &str) -> R<()> {
    let reason = format!("agent-task {id}");
    git::run(
        &repo.root,
        [
            OsStr::new("worktree"),
            OsStr::new("lock"),
            OsStr::new("--reason"),
            OsStr::new(&reason),
            path,
        ],
    )
    .map(|_| ())
}

fn cmd_create(a: &Args) -> R<()> {
    let slug = a.need("--slug")?;
    if !task::valid_slug(slug) {
        return Err(Fail::new(
            2,
            "--slug must be 2-4 lowercase ASCII words of letters and digits joined by hyphens",
        ));
    }
    let text = a.get("--task")?.unwrap_or("");
    if text.contains(['\n', '\r']) {
        return Err(Fail::new(2, "--task must be a single line"));
    }
    let from = a.get("--from")?.unwrap_or("refs/heads/main");
    if from.starts_with('-') {
        return Err(Fail::new(2, "--from must not start with '-'"));
    }
    // Resolve the owner first so a failed lookup never leaves an unowned worktree.
    let owner = me()?;
    let root = canon(Path::new(a.need_os("--repo")?));
    // One call resolves the common dir, hook dir, and source commit.
    let out = git::output(
        &root,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--git-path",
            "hooks",
            &format!("{from}^{{commit}}"),
        ],
    )?;
    if !out.status.success() {
        return Err(Fail::new(
            2,
            format!("cannot resolve --from {from:?} in {}", root.display()),
        ));
    }
    let lines: Vec<&[u8]> = out.stdout.split(|b| *b == b'\n').collect();
    let (Some(common), Some(hooks_dir), Some(base)) = (lines.first(), lines.get(1), lines.get(2))
    else {
        return Err(Fail::new(1, "unexpected git rev-parse output"));
    };
    let base = String::from_utf8_lossy(base).into_owned();
    if !git::is_oid(&base) {
        return Err(Fail::new(1, "unexpected git rev-parse output"));
    }
    let repo = Repo {
        root,
        common: canon(Path::new(OsStr::from_bytes(common))),
    };
    repo.require_annex_ready(a.flag("--external-writers-quiesced"))?;
    let hooks = active_hooks(
        &repo.root,
        &["post-checkout", "reference-transaction"],
        Some(&base),
        Some(Path::new(OsStr::from_bytes(hooks_dir))),
    )?;
    check_hooks(&hooks, a.flag("--allow-repository-hooks"))?;

    let root_dir = if let Some(custom) = a.os("--worktrees-root") {
        let custom = PathBuf::from(custom);
        let absolute = if custom.is_absolute() {
            custom
        } else {
            std::env::current_dir()
                .map_err(|error| Fail::new(1, format!("cannot resolve worktrees root: {error}")))?
                .join(custom)
        };
        canon(&absolute)
    } else {
        state_home()?.join(repo.name()).join("worktrees")
    };
    let parent = a.get("--parent")?;
    if parent.is_some_and(|value| value.is_empty() || value.contains(['\n', '\r'])) {
        return Err(Fail::new(2, "--parent must be a non-empty single line"));
    }
    let id = format!("{slug}-{}-{}", local_date(), random_hex(4)?);
    let path = root_dir.join(&id);
    let Some(path_text) = path.to_str().map(str::to_string) else {
        return Err(Fail::new(1, "the worktree path must be valid UTF-8"));
    };
    let admin = repo.common.join("worktrees").join(&id);
    if path.exists() || admin.exists() {
        return Err(Fail::new(1, format!("{id} already exists; retry")));
    }
    fs::create_dir_all(&root_dir)
        .map_err(|e| Fail::new(1, format!("{}: {e}", root_dir.display())))?;
    let reason = format!("agent-task {id}");
    git::run(
        &repo.root,
        [
            "worktree", "add", "--quiet", "--detach", "--lock", "--reason", &reason, &path_text,
            &base,
        ],
    )?;

    let gitdir_ok = task::registered_path(&admin) == Some(canon(&path));
    let session_var = match owner.client.as_str() {
        "claude" => "CLAUDE_CODE_SESSION_ID",
        "opencode" => "OPENCODE_SESSION_ID",
        _ => "",
    };
    let session_hint = match show::session_arg(a)? {
        Some(explicit) => Some(explicit),
        None => std::env::var(session_var).ok().filter(|v| !v.is_empty()),
    };
    let worktree = canon(&path).to_string_lossy().into_owned();
    let marker = json!({
        "schema": task::SCHEMA,
        "tool": "wtctl",
        "task_id": id,
        "worktree": worktree,
        "base": base,
        "owner": owner_json(&owner),
        "session_hint": session_hint,
        "task": text,
        "parent": parent,
        "created_unix": now(),
        "history": [],
    });
    let written = if gitdir_ok {
        task::write_marker(&admin, &marker)
    } else {
        Err(Fail::new(
            1,
            "Git admin directory does not match the new worktree",
        ))
    };
    if let Err(fail) = written {
        // The worktree is pristine and ours; an unowned locked worktree is worse than none.
        let _ = git::run(&repo.root, ["worktree", "unlock", &path_text]);
        let removed = git::run(&repo.root, ["worktree", "remove", &path_text]).is_ok();
        let state = if removed {
            "removed".to_string()
        } else {
            match relock(&repo, OsStr::new(&path_text), &id) {
                Ok(()) => "preserved and locked".to_string(),
                Err(e) => format!("preserved; worktree left UNLOCKED ({})", e.msg),
            }
        };
        return Err(Fail::new(1, format!("{}; worktree {state}", fail.msg)));
    }
    if a.flag("--json") {
        out!("{marker}");
    } else {
        out!(
            "task_id={id}\nworktree={worktree}\nbase={base}\nowner={} pid={}",
            owner.client,
            owner.pid
        );
        if let Some(session) = session_hint.as_deref() {
            out!("session={session}");
        }
    }
    Ok(())
}

fn cmd_whoami(a: &Args) -> R<()> {
    let path = match a.os("--path") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()
            .map_err(|e| Fail::new(1, format!("cannot read the current directory: {e}")))?,
    };
    let out = git::output(
        &path,
        [
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
        ],
    )?;
    if !out.status.success() {
        return Err(Fail::new(
            1,
            format!("{} is not in a Git worktree", path.display()),
        ));
    }
    let mut lines = out.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty());
    let (Some(top), Some(admin)) = (lines.next(), lines.next()) else {
        return Err(Fail::new(1, "unexpected git rev-parse output"));
    };
    let top = Path::new(OsStr::from_bytes(top));
    let marker = match task::read_meta(Path::new(OsStr::from_bytes(admin))) {
        Meta::Wtctl(m) => m,
        Meta::Legacy(t) => {
            return Err(Fail::new(
                1,
                format!(
                    "{} is a legacy task worktree (client={} session={}); not managed by wtctl",
                    top.display(),
                    t.get("client").map_or("?", String::as_str),
                    t.get("session").map_or("?", String::as_str)
                ),
            ))
        }
        Meta::Invalid(e) => return refuse(e),
        Meta::None => {
            return Err(Fail::new(
                1,
                format!("{} is not a wtctl task worktree", top.display()),
            ))
        }
    };
    if canon(&marker.worktree) != canon(top) {
        return refuse(format!(
            "marker names {} but this worktree is {}",
            marker.worktree.display(),
            top.display()
        ));
    }
    let current = procinfo::find_agent();
    let state = procinfo::liveness(&marker.owner);
    let mine = match &current {
        Some(_) if is_mine(&marker, current.as_ref()) => "yes",
        Some(_) => "no",
        None => "unknown",
    };
    if a.flag("--json") {
        out!(
            "{}",
            json!({"task_id": marker.task_id, "worktree": top.to_string_lossy(),
                   "base": marker.base, "owner": owner_json(&marker.owner),
                   "owner_state": state.as_str(), "mine": mine,
                   "session": marker.session_hint(),
                   "resume_command": show::resume_command(&marker)})
        );
    } else {
        out!(
            "task_id={}\nworktree={}\nbase={}\nowner={} pid={} state={}\nmine={mine}",
            marker.task_id,
            top.display(),
            marker.base,
            marker.owner.client,
            marker.owner.pid,
            state.as_str()
        );
        if let Some(session) = marker.session_hint() {
            out!("session={session}");
        }
        if let Some(command) = show::resume_command(&marker) {
            out!("resume={command}");
        }
    }
    Ok(())
}

fn legacy_metadata(repo: &Repo, entry: &Entry) -> Option<HashMap<String, String>> {
    match &entry.meta {
        Meta::Legacy(trailer) => Some(trailer.clone()),
        Meta::Wtctl(_) | Meta::Invalid(_) => None,
        Meta::None => {
            let branch = entry.branch_ref()?.strip_prefix("refs/heads/")?;
            let key = format!("branch.{branch}.description");
            let description = git::output(&repo.root, ["config", "--get", &key]).ok()?;
            description
                .status
                .success()
                .then(|| task::legacy_trailer(&String::from_utf8_lossy(&description.stdout)))
        }
    }
}

fn valid_legacy_session(client: &str, session: &str) -> bool {
    match client {
        "claude" => {
            session.len() == 36
                && session.bytes().enumerate().all(|(i, b)| match i {
                    8 | 13 | 18 | 23 => b == b'-',
                    _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
                })
        }
        "opencode" => session
            .strip_prefix("ses_")
            .is_some_and(|v| v.len() == 26 && v.bytes().all(|b| b.is_ascii_alphanumeric())),
        _ => false,
    }
}

fn current_session_identity() -> Option<(String, String)> {
    let present: Vec<(String, String)> = [
        ("claude", "CLAUDE_CODE_SESSION_ID"),
        ("opencode", "OPENCODE_SESSION_ID"),
    ]
    .into_iter()
    .filter_map(|(client, variable)| {
        std::env::var(variable)
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| (client.to_string(), value))
    })
    .collect();
    match present.as_slice() {
        [(client, session)] if valid_legacy_session(client, session) => {
            Some((client.clone(), session.clone()))
        }
        _ => None,
    }
}

fn legacy_is_mine(trailer: &HashMap<String, String>) -> bool {
    let (Some(client), Some(session)) = (trailer.get("client"), trailer.get("session")) else {
        return false;
    };
    let Some((current_client, current_session)) = current_session_identity() else {
        return false;
    };
    if !valid_legacy_session(client, session) {
        return false;
    }
    current_client == *client && current_session == *session
}

fn owner_state(
    meta: &Meta,
    legacy: Option<&HashMap<String, String>>,
    active_hours: f64,
    home: &Path,
    xdg_data: &Path,
    claude_config: Option<&Path>,
) -> (Liveness, Option<f64>) {
    match meta {
        Meta::Wtctl(m) => (procinfo::liveness(&m.owner), None),
        Meta::Legacy(t) => legacy_owner_state(t, active_hours, home, xdg_data, claude_config),
        _ => legacy.map_or((Liveness::Unknown, None), |t| {
            legacy_owner_state(t, active_hours, home, xdg_data, claude_config)
        }),
    }
}

fn legacy_owner_state(
    trailer: &HashMap<String, String>,
    active_hours: f64,
    home: &Path,
    xdg_data: &Path,
    claude_config: Option<&Path>,
) -> (Liveness, Option<f64>) {
    if task::legacy_liveness(trailer) == Liveness::Alive {
        return (Liveness::Alive, None);
    }
    let (Some(client), Some(session)) = (trailer.get("client"), trailer.get("session")) else {
        return (Liveness::Unknown, None);
    };
    let Some(idle_hours) =
        legacy::session_idle_hours(client, session, home, xdg_data, claude_config, now() as f64)
    else {
        return (Liveness::Unknown, None);
    };
    (
        if idle_hours < active_hours {
            Liveness::Alive
        } else {
            Liveness::Dead
        },
        Some(idle_hours),
    )
}

fn legacy_audit_state(
    repo: &Repo,
    entry: &Entry,
    trailer: Option<&HashMap<String, String>>,
    owner: Liveness,
    facts: &task::Facts,
    main_ref: &str,
) -> String {
    match owner {
        Liveness::Alive => return "ACTIVE".into(),
        Liveness::Unknown => return "UNKNOWN".into(),
        Liveness::Dead => {}
    }
    let Some(trailer) = trailer else {
        return "UNKNOWN".into();
    };
    let (Some(client), Some(session)) = (trailer.get("client"), trailer.get("session")) else {
        return "UNKNOWN".into();
    };
    let Some(current) = current_session_identity() else {
        return "IDLE-OWNER-UNKNOWN".into();
    };
    if current.0 != *client || current.1 != *session {
        return "IDLE-OTHER-SESSION".into();
    }
    let Some(base) = trailer.get("base").filter(|oid| git::is_oid(oid)) else {
        return "IDLE-UNMERGED".into();
    };
    let Some(head) = entry.current_oid() else {
        return "IDLE-UNMERGED".into();
    };
    let ahead = head != *base && is_ancestor(&repo.root, base, &head).unwrap_or(false);
    let contained =
        facts.unintegrated == Some(0) && is_ancestor(&repo.root, &head, main_ref).unwrap_or(false);
    let clean = facts
        .clean
        .is_some_and(|state| state.tracked == 0 && state.untracked == 0);
    if ahead && contained && clean {
        "IDLE-MERGED".into()
    } else {
        "IDLE-UNMERGED".into()
    }
}

fn cmd_list(a: &Args) -> R<()> {
    let repo = a.repo(false)?;
    if a.flag("--abandoned") {
        return abandon::list(a, &repo);
    }
    let (_, main_ref) = base_ref(a, &repo.root)?;
    let modes: Vec<&str> = ["--mine", "--orphans", "--all", "--wtctl", "--legacy"]
        .into_iter()
        .filter(|f| a.flag(f))
        .collect();
    if modes.len() > 1 {
        return Err(Fail::new(
            2,
            "choose one of --mine, --orphans, --all, --wtctl, --legacy",
        ));
    }
    let mode = modes.first().copied().unwrap_or("--wtctl");
    let active_hours = match a.get("--active-hours")? {
        Some(value) => value
            .parse::<f64>()
            .ok()
            .filter(|hours| hours.is_finite() && *hours >= 0.0)
            .ok_or_else(|| Fail::new(2, "--active-hours must be a non-negative number"))?,
        None => 2.0,
    };
    let current = if mode == "--mine" {
        Some(me()?)
    } else {
        procinfo::find_agent()
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let xdg_data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    let claude_config = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let all = task::entries(&repo.common);
    let legacy: HashMap<PathBuf, HashMap<String, String>> = all
        .iter()
        .filter_map(|entry| legacy_metadata(&repo, entry).map(|meta| (entry.admin.clone(), meta)))
        .collect();
    let selected: Vec<&Entry> = all
        .iter()
        .filter(|e| match mode {
            "--all" => true,
            "--wtctl" => e.marker().is_some(),
            "--legacy" => e.marker().is_none() && legacy.contains_key(&e.admin),
            "--mine" => e.marker().map_or_else(
                || legacy.get(&e.admin).is_some_and(legacy_is_mine),
                |m| is_mine(m, current.as_ref()),
            ),
            "--orphans" => e
                .marker()
                .is_some_and(|m| procinfo::liveness(&m.owner) == Liveness::Dead),
            _ => false,
        })
        .collect();
    let facts = task::facts(&selected, &main_ref)?;
    let main_worktree = canonical_main_checkout(&repo)?;
    let mut rows = Vec::new();
    for (e, f) in selected.iter().zip(&facts) {
        let legacy_row = legacy.get(&e.admin);
        let (state, idle_hours) = owner_state(
            &e.meta,
            legacy_row,
            active_hours,
            &home,
            &xdg_data,
            claude_config.as_deref(),
        );
        let (kind, owner, mine) = match &e.meta {
            Meta::Wtctl(m) => (
                "wtctl",
                format!("{}:{}", m.owner.client, m.owner.pid),
                is_mine(m, current.as_ref()),
            ),
            Meta::Legacy(t) => (
                "legacy",
                format!(
                    "{}:{}",
                    t.get("client").map_or("?", String::as_str),
                    t.get("session").map_or("?", String::as_str)
                ),
                legacy_is_mine(t),
            ),
            Meta::Invalid(_) => ("invalid", String::new(), false),
            Meta::None => match legacy_row {
                Some(t) => (
                    "legacy",
                    format!(
                        "{}:{}",
                        t.get("client").map_or("?", String::as_str),
                        t.get("session").map_or("?", String::as_str)
                    ),
                    legacy_is_mine(t),
                ),
                None => ("none", String::new(), false),
            },
        };
        let audit_state = if kind == "legacy" {
            legacy_audit_state(&repo, e, legacy_row, state, f, &main_ref)
        } else if kind == "wtctl" {
            match state {
                Liveness::Alive => "ACTIVE".into(),
                Liveness::Unknown => "UNKNOWN".into(),
                Liveness::Dead => "ORPHAN".into(),
            }
        } else {
            "UNKNOWN".into()
        };
        let facts_removable = f.removable();
        let artifact_blocker = if facts_removable.is_ok() && e.path.is_dir() {
            artifact::check_pending_transaction(&repo.name(), &e.path)
                .and_then(|()| {
                    artifact::inspect_plan(
                        &e.path,
                        main_worktree.as_deref(),
                        artifact::Options::default(),
                    )
                    .map(|_| ())
                })
                .err()
        } else {
            None
        };
        let removable = if kind == "legacy" && audit_state != "IDLE-MERGED" {
            Value::String(format!("legacy audit state is {audit_state}"))
        } else if let Err(why) = facts_removable {
            Value::String(why)
        } else if let Some(why) = artifact_blocker.clone() {
            Value::String(why)
        } else {
            Value::Bool(true)
        };
        let suggested_remove = if kind == "legacy"
            && audit_state == "IDLE-MERGED"
            && mine
            && artifact_blocker.is_none()
            && f.removable().is_ok()
        {
            let selector =
                if let Some(branch) = e.branch_ref().and_then(|r| r.strip_prefix("refs/heads/")) {
                    vec!["--expect-branch".to_string(), branch.to_string()]
                } else {
                    vec!["--legacy-worktree-id".to_string(), e.id()]
                };
            let mut command = vec![
                "wtctl".to_string(),
                "remove".to_string(),
                "--repo".to_string(),
                repo.root.to_string_lossy().into_owned(),
            ];
            command.extend(selector);
            if let Some(base) = a.get("--base")? {
                command.extend(["--base".into(), base.to_string()]);
            } else if let Some(main) = a.get("--main")? {
                command.extend(["--main".into(), main.to_string()]);
            }
            command.push("--external-writers-quiesced".into());
            command.push("--dry-run".into());
            Some(command)
        } else {
            None
        };
        rows.push(json!({
            "id": e.id(), "kind": kind, "path": e.path.to_string_lossy(), "head": e.head,
            "branch": e.branch_ref().and_then(|r| r.strip_prefix("refs/heads/")),
            "task": e.marker().and_then(|m| m.raw.get("task")).and_then(Value::as_str),
            "session": e.marker().and_then(Marker::session_hint),
            "locked": e.lock_reason.is_some(), "lock_reason": e.lock_reason, "owner": owner, "owner_state": state.as_str(), "session_idle_hours": idle_hours, "audit_state": audit_state,
            "mine": mine, "exists": e.path.is_dir(),
            "clean": f.clean.map(|c| c.is_clean()),
            "changes": f.clean.map(|c| json!({"tracked": c.tracked, "untracked": c.untracked, "ignored": c.ignored})),
            "hidden": f.hidden, "in_progress": f.in_progress, "blocker": f.blocker,
            "unintegrated": f.unintegrated, "integrated": f.unintegrated.map(|n| n == 0),
            "removable": removable,
            "suggested_remove": suggested_remove,
        }));
    }
    if a.flag("--json") {
        out!("{}", Value::Array(rows));
        return Ok(());
    }
    let yn = |v: &Value| match v.as_bool() {
        Some(true) => "yes",
        Some(false) => "no",
        None => "?",
    };
    for r in &rows {
        let idle = r["session_idle_hours"]
            .as_f64()
            .map_or_else(|| "?".into(), |hours| format!("{hours:.1}h"));
        out!(
            "{}\t{}\towner={} {}{} session_idle={} audit={}\tintegrated={} unintegrated={}\tclean={}\tlocked={}\t{}",
            r["id"].as_str().unwrap_or(""),
            r["kind"].as_str().unwrap_or(""),
            r["owner"].as_str().unwrap_or(""),
            r["owner_state"].as_str().unwrap_or(""),
            if r["mine"].as_bool() == Some(true) {
                " (mine)"
            } else {
                ""
            },
            idle,
            r["audit_state"].as_str().unwrap_or("UNKNOWN"),
            yn(&r["integrated"]),
            r["unintegrated"]
                .as_u64()
                .map_or("?".into(), |n| n.to_string()),
            yn(&r["clean"]),
            yn(&r["locked"]),
            r["path"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

fn require_owner(marker: &Marker) -> R<Owner> {
    let current = me()?;
    if !is_mine(marker, Some(&current)) {
        let state = match procinfo::liveness(&marker.owner) {
            Liveness::Alive => "alive",
            Liveness::Dead => "dead; adopt it first",
            Liveness::Unknown => "liveness cannot be verified",
        };
        return refuse(format!(
            "{} is owned by {} pid {} ({state}), not by the calling agent pid {}",
            marker.task_id, marker.owner.client, marker.owner.pid, current.pid
        ));
    }
    Ok(current)
}

fn ensure_ref(repo: &Path, refname: &str, expected: &str) -> R<()> {
    let actual = git::run(repo, ["rev-parse", "--verify", "--quiet", refname])?;
    if actual.trim() != expected {
        return refuse(format!(
            "{refname} moved from {expected} to {}; retry",
            actual.trim()
        ));
    }
    Ok(())
}

fn ensure_task_head(entry: &Entry, expected: &str) -> R<()> {
    if entry.current_oid().as_deref() != Some(expected) {
        return refuse(format!("{} HEAD changed during integration", entry.id()));
    }
    Ok(())
}

fn ensure_no_cwd_users(entry: &Entry, stage: &str) -> R<()> {
    let users = procinfo::users_inside(&procinfo::scan_cwds(), &entry.path).map_err(|error| {
        Fail::new(
            4,
            format!(
                "cannot inspect processes inside {}: {error}",
                entry.path.display()
            ),
        )
    })?;
    if !users.is_empty() {
        return refuse(format!(
            "processes are using {} {stage} (pids: {})",
            entry.path.display(),
            users.join(", ")
        ));
    }
    Ok(())
}

fn cas_update(repo: &Path, refname: &str, new: &str, old: &str, msg: &str) -> R<()> {
    let out = git::output(repo, ["update-ref", "-m", msg, refname, new, old])?;
    if !out.status.success() {
        return refuse(format!(
            "compare-and-swap of {refname} failed: {}",
            git::stderr_of(&out)
        ));
    }
    Ok(())
}

fn is_ancestor(repo: &Path, a: &str, b: &str) -> R<bool> {
    let out = git::output(repo, ["merge-base", "--is-ancestor", a, b])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Fail::new(1, git::stderr_of(&out))),
    }
}

fn main_checkout(repo: &Repo, refname: &str) -> R<Option<PathBuf>> {
    let out = git::output(&repo.root, ["worktree", "list", "--porcelain", "-z"])?;
    if !out.status.success() {
        return Err(Fail::new(1, git::stderr_of(&out)));
    }
    let mut path: Option<&[u8]> = None;
    for field in out.stdout.split(|b| *b == 0) {
        if let Some(p) = field.strip_prefix(b"worktree ") {
            path = Some(p);
        } else if field.strip_prefix(b"branch ") == Some(refname.as_bytes()) {
            return Ok(path.map(|p| PathBuf::from(OsStr::from_bytes(p))));
        }
    }
    Ok(None)
}

fn canonical_main_checkout(repo: &Repo) -> R<Option<PathBuf>> {
    main_checkout(repo, "refs/heads/main")
}

fn gitlinks(repo: &Path, treeish: &str) -> R<BTreeMap<Vec<u8>, Vec<u8>>> {
    let output = git::output(repo, ["ls-tree", "-rz", "--full-tree", treeish])?;
    if !output.status.success() {
        return refuse(format!("cannot inspect submodule entries in {treeish}"));
    }
    let mut links = BTreeMap::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return refuse(format!(
                "git ls-tree returned malformed output for {treeish}"
            ));
        };
        let metadata: Vec<&[u8]> = record[..tab].split(|byte| *byte == b' ').collect();
        if metadata.len() == 3 && metadata[0] == b"160000" {
            links.insert(record[tab + 1..].to_vec(), metadata[2].to_vec());
        }
    }
    Ok(links)
}

fn require_no_gitlink_changes(repo: &Path, base: &str, tip: &str) -> R<()> {
    let before = gitlinks(repo, base)?;
    let after = gitlinks(repo, tip)?;
    let changed: Vec<_> = before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    if !changed.is_empty() {
        return refuse(format!(
            "submodule gitlink changes require a separate task: {}",
            changed.join(", ")
        ));
    }
    Ok(())
}

struct IntegrationContext<'a> {
    id: &'a str,
    main: &'a str,
    refname: &'a str,
    task_base: &'a str,
}

struct IntegrationOptions {
    allow_hooks: bool,
    allow_annex_filter: bool,
    cleanup: Option<artifact::Options>,
    json_output: bool,
}

fn integrate_task(
    repo: &Repo,
    context: IntegrationContext<'_>,
    entry: &Entry,
    caller: &Owner,
    options: IntegrationOptions,
) -> R<(String, String, bool)> {
    let IntegrationContext {
        id,
        main,
        refname,
        task_base,
    } = context;
    let IntegrationOptions {
        allow_hooks,
        allow_annex_filter,
        cleanup,
        json_output,
    } = options;
    let tip = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, format!("cannot resolve {id} HEAD")))?;
    let tip_tree = format!("{tip}^{{tree}}");
    verify_tracked_filter_safety(repo, &entry.path, &tip_tree, allow_annex_filter)?;
    require_no_gitlink_changes(&repo.root, task_base, &tip)?;
    let facts = task::facts(&[entry], refname)?;
    let f = facts.first().cloned().unwrap_or_default();
    if let Some(op) = f.in_progress {
        return refuse(format!("{id} has a {op} in progress"));
    }
    if let Some(cleanup) = cleanup {
        f.removable_except_integration()
            .map_err(|why| Fail::new(4, format!("{id} has unsafe state: {why}")))?;
        let main_worktree = canonical_main_checkout(repo)?;
        artifact::inspect_plan(&entry.path, main_worktree.as_deref(), cleanup)
            .map_err(|why| Fail::new(4, format!("{id} has changes: {why}")))?;
    } else {
        match f.clean {
            Some(c) if c.is_clean() => {}
            Some(c) => {
                return refuse(format!(
                    "{id} has changes ({} tracked, {} untracked, {} ignored); clean or explicitly preserve them first",
                    c.tracked, c.untracked, c.ignored
                ))
            }
            None => return refuse(format!("cannot read the status of {id}")),
        }
    }
    if f.hidden != Some(0) {
        return refuse(format!("{id} has assume-unchanged/skip-worktree entries"));
    }
    let old = git::run(&repo.root, ["rev-parse", "--verify", "--quiet", refname])
        .map_err(|_| Fail::new(1, format!("{refname} does not exist")))?
        .trim()
        .to_string();

    if is_ancestor(&repo.root, &tip, &old)? {
        return Ok((old.clone(), old, true));
    }
    ensure_task_head(entry, &tip)?;
    ensure_no_cwd_users(entry, "integration")?;
    check_active_filters(repo, &entry.path, &old, &tip, allow_annex_filter)?;
    if !is_ancestor(&repo.root, &old, &tip)? {
        let mt = git::output(
            &repo.root,
            [
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                &old,
                &tip,
            ],
        )?;
        let text = String::from_utf8_lossy(&mt.stdout);
        let conflicts: Vec<&str> = text.lines().skip(1).filter(|l| !l.is_empty()).collect();
        let detail = match mt.status.code() {
            Some(0) => "no conflicts expected".to_string(),
            Some(1) => format!("conflicts: {}", conflicts.join(", ")),
            _ => format!("merge-tree failed: {}", git::stderr_of(&mt)),
        };
        if !json_output {
            for path in &conflicts {
                out!("conflict\t{path}");
            }
        }
        return Err(Fail::new(
            3,
            format!("{main} advanced past the task base; merge {main} into the {id} worktree, commit, and retry; do not rebase ({detail})"),
        ));
    }

    let msg = format!("wtctl integrate {id}");
    ensure_ref(&repo.root, refname, &old)?;
    ensure_task_head(entry, &tip)?;
    ensure_no_cwd_users(entry, "integration")?;
    match main_checkout(repo, refname)? {
        Some(checkout) => {
            check_active_filters(repo, &checkout, &old, &tip, allow_annex_filter)?;
            check_hooks(
                &active_hooks(
                    &checkout,
                    &["post-merge", "reference-transaction"],
                    None,
                    None,
                )?,
                allow_hooks,
            )?;
            let st = git::output(
                &checkout,
                ["status", "--porcelain", "-z", "--untracked-files=normal"],
            )?;
            if !st.status.success() || !st.stdout.is_empty() {
                return refuse(format!("{} is not clean", checkout.display()));
            }
            preflight::check_merge_paths(&checkout, &old, &tip).map_err(|why| {
                Fail::new(4, format!("main fast-forward path preflight failed: {why}"))
            })?;
            ensure_ref(&repo.root, refname, &old)?;
            let out = git::output(
                &checkout,
                [
                    "merge",
                    "--ff-only",
                    "--no-overwrite-ignore",
                    "--quiet",
                    &tip,
                ],
            )?;
            if !out.status.success() {
                return refuse(format!("fast-forward refused: {}", git::stderr_of(&out)));
            }
        }
        None => {
            check_hooks(
                &active_hooks(&repo.root, &["reference-transaction"], None, None)?,
                allow_hooks,
            )?;
            cas_update(&repo.root, refname, &tip, &old, &msg)?;
        }
    }
    ensure_ref(&repo.root, refname, &tip)?;
    repo.audit(
        json!({"at": now(), "event": "integrate", "task_id": id, "ref": refname,
                      "old": old, "new": tip, "pid": caller.pid}),
    );
    Ok((old, tip, false))
}

fn cmd_integrate(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let id = a.need("--id")?;
    if !a.flag("--external-writers-quiesced") {
        return refuse(
            "inspect repository-specific writers, then pass --external-writers-quiesced",
        );
    }
    let (main, refname) = main_ref(a, &repo.root)?;
    repo.require_annex_ready(a.flag("--external-writers-quiesced"))?;
    let _lock = repo.lock()?;
    let (entry, marker) = repo.find_task(id)?;
    let caller = require_owner(&marker)?;
    let (old, tip, already) = integrate_task(
        &repo,
        IntegrationContext {
            id,
            main: &main,
            refname: &refname,
            task_base: &marker.base,
        },
        &entry,
        &caller,
        IntegrationOptions {
            allow_hooks: a.flag("--allow-repository-hooks"),
            allow_annex_filter: a.flag("--allow-annex-filter"),
            cleanup: None,
            json_output: a.flag("--json"),
        },
    )?;
    if a.flag("--json") {
        out!(
            "{}",
            json!({
                "status": if already { "already-integrated" } else { "integrated" },
                "task_id": id,
                "main": main,
                "old": old,
                "new": tip,
            })
        );
    } else if already {
        out!("{id} is already integrated into {main} ({old})");
    } else {
        out!("integrated {id}: {main} {old}..{tip}");
    }
    Ok(())
}

struct FinishMerge<'a> {
    main_ref: &'a str,
    main_tip: &'a str,
    task_tip: &'a str,
    allow_hooks: bool,
    annex: bool,
    cleanup: artifact::Options,
    integration_message: &'a str,
    allow_annex_filter: bool,
    allow_commit_signing: bool,
}

fn merge_main_into_task(
    repo: &Repo,
    id: &str,
    entry: &Entry,
    task_base: &str,
    context: FinishMerge<'_>,
) -> R<String> {
    ensure_task_head(entry, context.task_tip)?;
    ensure_no_cwd_users(entry, "merge preparation")?;
    let task_tree = format!("{}^{{tree}}", context.task_tip);
    verify_tracked_filter_safety(repo, &entry.path, &task_tree, context.allow_annex_filter)?;
    if !is_ancestor(&repo.root, task_base, context.main_tip)?
        || !is_ancestor(&repo.root, task_base, context.task_tip)?
    {
        return refuse(format!("{id} and main do not share the recorded task base"));
    }
    let initial = task::facts(&[entry], context.main_ref)?;
    let initial = initial.first().cloned().unwrap_or_default();
    initial
        .removable_except_integration()
        .map_err(|why| Fail::new(4, format!("{id} is not safe to merge: {why}")))?;
    let main_worktree = canonical_main_checkout(repo)?;
    let initial_plan =
        artifact::inspect_plan(&entry.path, main_worktree.as_deref(), context.cleanup).map_err(
            |why| {
                Fail::new(
                    4,
                    format!("{id} must be clean before merging advanced main: {why}"),
                )
            },
        )?;
    if let Some(operation) = initial.in_progress {
        return refuse(format!("{id} has a {operation} in progress"));
    }
    let preview = git::output(
        &repo.root,
        [
            "merge-tree",
            "--write-tree",
            "--no-messages",
            context.task_tip,
            context.main_tip,
        ],
    )?;
    let expected_tree = match preview.status.code() {
        Some(0) => {
            let merged_tree = String::from_utf8_lossy(&preview.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !git::is_oid(&merged_tree) {
                return refuse("merge-tree returned no valid result tree");
            }
            preflight::check_merge_paths(&entry.path, context.task_tip, &merged_tree)
                .map_err(|why| Fail::new(4, format!("final merge tree is unsafe: {why}")))?;
            check_commit_signing(repo, context.allow_commit_signing)?;
            check_active_filters(
                repo,
                &entry.path,
                context.task_tip,
                &merged_tree,
                context.allow_annex_filter,
            )?;
            merged_tree
        }
        Some(1) => {
            return Err(Fail::new(
                3,
                format!(
                    "merge of advanced main into {id} conflicts; task worktree preserved for resolution: {}",
                    String::from_utf8_lossy(&preview.stdout).trim().replace('\n', ", ")
                ),
            ));
        }
        _ => {
            return refuse(format!(
                "merge-tree preflight failed: {}",
                git::stderr_of(&preview)
            ));
        }
    };
    verify_tracked_filter_safety(repo, &entry.path, &task_tree, context.allow_annex_filter)?;
    let hooks = active_hooks(
        &entry.path,
        &[
            "pre-merge-commit",
            "commit-msg",
            "post-merge",
            "reference-transaction",
        ],
        None,
        None,
    )?;
    check_hooks(&hooks, context.allow_hooks)?;
    ensure_ref(&repo.root, context.main_ref, context.main_tip)?;
    ensure_task_head(entry, context.task_tip)?;
    ensure_no_cwd_users(entry, "merge")?;
    ensure_no_merge_autostash(&entry.path).map_err(|why| {
        Fail::new(
            4,
            format!("{id} cannot merge with MERGE_AUTOSTASH present: {why}"),
        )
    })?;
    ensure_visible_index_flags(&entry.path)
        .map_err(|why| Fail::new(4, format!("{id} index state is unsafe to merge: {why}")))?;
    if context.annex {
        annex_restage(
            entry,
            &entry.head,
            main_worktree.as_deref(),
            context.cleanup,
            &initial_plan,
        )?;
    }
    ensure_ref(&repo.root, context.main_ref, context.main_tip)?;
    ensure_task_head(entry, context.task_tip)?;
    ensure_no_cwd_users(entry, "merge preparation")?;
    let merge_plan = artifact::inspect_plan(&entry.path, main_worktree.as_deref(), context.cleanup)
        .map_err(|why| Fail::new(4, format!("{id} changed during merge preparation: {why}")))?;
    if merge_plan != initial_plan {
        return refuse(format!(
            "{id} cleanup artifact plan changed during merge preparation"
        ));
    }
    preflight::check_merge_paths(&entry.path, context.task_tip, &expected_tree).map_err(|why| {
        Fail::new(
            4,
            format!("final merge tree is unsafe after restage: {why}"),
        )
    })?;
    check_active_filters(
        repo,
        &entry.path,
        context.task_tip,
        &expected_tree,
        context.allow_annex_filter,
    )?;
    verify_tracked_filter_safety(repo, &entry.path, &task_tree, context.allow_annex_filter)?;
    ensure_ref(&repo.root, context.main_ref, context.main_tip)?;
    ensure_task_head(entry, context.task_tip)?;
    ensure_no_cwd_users(entry, "merge")?;
    ensure_no_merge_autostash(&entry.path).map_err(|why| {
        Fail::new(
            4,
            format!("{id} cannot merge with MERGE_AUTOSTASH present: {why}"),
        )
    })?;
    ensure_visible_index_flags(&entry.path)
        .map_err(|why| Fail::new(4, format!("{id} index state is unsafe to merge: {why}")))?;
    let refs_before = git::run(
        &repo.root,
        ["for-each-ref", "--format=%(refname) %(objectname)"],
    )?;
    let extras_before = extra_status_entries(&entry.path).map_err(|why| {
        Fail::new(
            4,
            format!("cannot snapshot untracked worktree paths before merge: {why}"),
        )
    })?;
    let merge = git::output(
        &entry.path,
        [
            "merge",
            "--no-edit",
            "--no-ff",
            "--no-overwrite-ignore",
            "-m",
            context.integration_message,
            context.main_tip,
        ],
    )?;
    if !merge.status.success() {
        let recovery = abort_failed_merge(
            repo,
            entry,
            context.task_tip,
            context.main_ref,
            context.main_tip,
            &expected_tree,
            &refs_before,
            &extras_before,
            main_worktree.as_deref(),
            &merge_plan,
            context.allow_annex_filter,
        );
        let (status, detail) = match recovery {
            Ok(()) => (
                "merge was aborted and task worktree restored",
                git::stderr_of(&merge),
            ),
            Err(reason) => (
                "task worktree preserved; automatic recovery was not completed",
                format!(
                    "starting task HEAD {}, {}={}; {reason}; {}; original merge error: {}",
                    context.task_tip,
                    context.main_ref,
                    context.main_tip,
                    merge_state_diagnostic(entry),
                    git::stderr_of(&merge)
                ),
            ),
        };
        return Err(Fail::new(
            3,
            format!(
                "merge of advanced main into {id} failed; {status} ({}): {detail}",
                entry.path.display(),
            ),
        ));
    }
    let merged_tip = git::run(&entry.path, ["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let merged_plan =
        artifact::inspect_plan(&entry.path, main_worktree.as_deref(), context.cleanup)
            .map_err(|why| Fail::new(4, format!("{id} changed before annex restage: {why}")))?;
    if context.annex {
        annex_restage(
            entry,
            &merged_tip,
            main_worktree.as_deref(),
            context.cleanup,
            &merged_plan,
        )?;
    }
    ensure_ref(&repo.root, context.main_ref, context.main_tip)?;
    let actual_tree = git::run(
        &entry.path,
        ["rev-parse", &format!("{merged_tip}^{{tree}}")],
    )?
    .trim()
    .to_string();
    if actual_tree != expected_tree {
        return refuse(format!(
            "{id} merge result tree differs from the preflight tree"
        ));
    }
    if !is_ancestor(&repo.root, context.task_tip, &merged_tip)?
        || !is_ancestor(&repo.root, context.main_tip, &merged_tip)?
    {
        return refuse(format!(
            "{id} merge commit does not preserve both histories"
        ));
    }
    let parents = git::run(&entry.path, ["show", "-s", "--format=%P", &merged_tip])?;
    let parents: Vec<_> = parents.split_whitespace().collect();
    if parents != [context.task_tip, context.main_tip] {
        return refuse(format!("{id} HEAD changed unexpectedly during merge"));
    }
    ensure_no_cwd_users(entry, "post-merge check")?;
    preflight::check_merge_paths(&entry.path, context.task_tip, &merged_tip)
        .map_err(|why| Fail::new(4, format!("merged tree changed after preflight: {why}")))?;
    let refreshed = task::facts(&[entry], context.main_ref)?;
    let refreshed = refreshed.first().cloned().unwrap_or_default();
    if refreshed.removable_except_integration().is_err()
        || artifact::inspect_plan(&entry.path, main_worktree.as_deref(), context.cleanup).is_err()
    {
        return refuse(format!(
            "{id} changed after merging main; leave it for inspection"
        ));
    }
    Ok(merged_tip)
}

#[allow(clippy::too_many_arguments)]
fn abort_failed_merge(
    repo: &Repo,
    entry: &Entry,
    task_tip: &str,
    main_ref: &str,
    main_tip: &str,
    expected_tree: &str,
    refs_before: &str,
    extras_before: &[git::StatusEntry],
    main_worktree: Option<&Path>,
    merge_plan: &artifact::Plan,
    allow_annex_filter: bool,
) -> Result<(), String> {
    let refuse = |reason: &str| Err(format!("recovery safety check failed: {reason}"));
    ensure_no_cwd_users(entry, "failed merge recovery").map_err(|error| error.msg)?;
    let head = git::run(&entry.path, ["rev-parse", "HEAD"]).map_err(|error| error.msg)?;
    if head.trim() != task_tip {
        return refuse("task HEAD changed during the failed merge");
    }
    if let Err(reason) = ensure_no_merge_autostash(&entry.path) {
        return refuse(&reason);
    }
    if let Err(reason) = ensure_visible_index_flags(&entry.path) {
        return refuse(&reason);
    }
    let orig_head = read_pseudo_ref(&entry.path, "ORIG_HEAD")?;
    if orig_head.as_deref() != Some(format!("{task_tip}\n").as_bytes()) {
        return refuse("ORIG_HEAD does not match the task commit used to start the merge");
    }
    let merge_head = read_pseudo_ref(&entry.path, "MERGE_HEAD")?
        .ok_or_else(|| "recovery safety check failed: MERGE_HEAD is missing".to_string())?;
    if merge_head != format!("{main_tip}\n").as_bytes() {
        return refuse("MERGE_HEAD is missing or does not contain only the expected main commit");
    }
    let unmerged =
        git::output(&entry.path, ["ls-files", "--unmerged"]).map_err(|error| error.msg)?;
    if !unmerged.status.success() || !unmerged.stdout.is_empty() {
        return refuse("the merge index contains unresolved entries");
    }
    if !index_matches_tree(&entry.path, expected_tree)? {
        return refuse("the merge index differs from the preflight tree");
    }
    verify_tracked_filter_safety(repo, &entry.path, expected_tree, allow_annex_filter)
        .map_err(|error| format!("recovery safety check failed: {}", error.msg))?;
    if !tracked_worktree_matches_index(repo, &entry.path, expected_tree, allow_annex_filter)? {
        return refuse("tracked worktree files differ from the merge index");
    }
    let restore_paths =
        verify_abort_restore_paths(&entry.path, task_tip, expected_tree, extras_before)?;
    let task_tree = format!("{task_tip}^{{tree}}");
    verify_filter_paths(
        repo,
        &entry.path,
        &restore_paths,
        &task_tree,
        allow_annex_filter,
        "task-tip paths restored by abort",
    )
    .map_err(|error| format!("recovery safety check failed: {}", error.msg))?;
    if extra_status_entries(&entry.path)? != extras_before {
        return refuse("untracked or ignored worktree paths changed during the failed merge");
    }
    if artifact::verify_plan_artifacts(merge_plan, &entry.path, main_worktree).is_err() {
        return refuse("untracked or ignored worktree artifacts changed during the failed merge");
    }
    let current_main =
        git::run(&repo.root, ["rev-parse", "--verify", main_ref]).map_err(|error| error.msg)?;
    if current_main.trim() != main_tip {
        return refuse("main moved during the failed merge");
    }
    let refs_now = git::run(
        &repo.root,
        ["for-each-ref", "--format=%(refname) %(objectname)"],
    )
    .map_err(|error| error.msg)?;
    if refs_now != refs_before {
        return refuse("Git refs changed during the failed merge");
    }
    ensure_no_cwd_users(entry, "failed merge abort").map_err(|error| error.msg)?;
    let open_users = worktree_open_path_users(entry, extras_before, merge_plan)?;
    if !open_users.is_empty() {
        return refuse(&format!(
            "processes have open handles in the task worktree (pids: {})",
            open_users.join(", ")
        ));
    }

    let abort = git::output(&entry.path, ["merge", "--abort"]).map_err(|error| error.msg)?;
    if !abort.status.success() {
        return Err(format!(
            "git merge --abort failed: {}",
            git::stderr_of(&abort)
        ));
    }
    let restored_head = git::run(&entry.path, ["rev-parse", "HEAD"]).map_err(|error| error.msg)?;
    if restored_head.trim() != task_tip {
        return Err("abort completed but task HEAD does not match its starting commit".into());
    }
    ensure_no_merge_autostash(&entry.path)
        .map_err(|reason| format!("abort completed but {reason}"))?;
    ensure_visible_index_flags(&entry.path)
        .map_err(|reason| format!("abort completed but {reason}"))?;
    let restored_orig_head = read_pseudo_ref(&entry.path, "ORIG_HEAD")?;
    if restored_orig_head.as_deref() != Some(format!("{task_tip}\n").as_bytes()) {
        return Err("abort completed but ORIG_HEAD does not match the starting task commit".into());
    }
    ensure_pseudo_ref_absent(&entry.path, "MERGE_HEAD")
        .map_err(|reason| format!("abort completed but {reason}"))?;
    let original_tree = format!("{task_tip}^{{tree}}");
    if !index_matches_tree(&entry.path, &original_tree)? {
        return Err("abort completed but the index differs from the starting tree".into());
    }
    if !tracked_worktree_matches_index(
        repo,
        &entry.path,
        &format!("{task_tip}^{{tree}}"),
        allow_annex_filter,
    )? {
        return Err(
            "abort completed but tracked worktree files differ from the starting tree".into(),
        );
    }
    let refs_after = git::run(
        &repo.root,
        ["for-each-ref", "--format=%(refname) %(objectname)"],
    )
    .map_err(|error| error.msg)?;
    if refs_after != refs_before {
        return Err("abort completed but Git refs differ from their starting state".into());
    }
    if extra_status_entries(&entry.path)? != extras_before {
        return Err("abort completed but untracked or ignored worktree paths changed".into());
    }
    if artifact::verify_plan_artifacts(merge_plan, &entry.path, main_worktree).is_err() {
        return Err(
            "abort completed but worktree artifacts differ from their starting state".into(),
        );
    }
    if !worktree_open_path_users(entry, extras_before, merge_plan)?.is_empty() {
        return Err(
            "abort completed but a process still has open handles in the task worktree".into(),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recover_pending_merge(
    repo: &Repo,
    id: &str,
    entry: &Entry,
    main_ref: &str,
    main_tip: &str,
    task_base: &str,
    task_tip: &str,
    allow_annex_filter: bool,
    external_writers_quiesced: bool,
) -> R<()> {
    let refuse = |reason: &str| {
        Fail::new(
            4,
            format!("{id} failed-merge recovery stopped before abort: {reason}"),
        )
    };
    if !external_writers_quiesced {
        return Err(refuse(
            "inspect scheduled and non-cooperating writers, then pass --external-writers-quiesced",
        ));
    }
    ensure_no_cwd_users(entry, "failed-merge recovery preparation")?;
    ensure_task_head(entry, task_tip)?;
    let task_tree = format!("{task_tip}^{{tree}}");
    verify_tracked_filter_safety(repo, &entry.path, &task_tree, allow_annex_filter)
        .map_err(|error| refuse(&error.msg))?;
    let head = git::run(&entry.path, ["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    if head != task_tip {
        return Err(refuse("task HEAD differs from its registered tip"));
    }
    for operation in [
        "rebase-merge",
        "rebase-apply",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "BISECT_START",
        "sequencer",
    ] {
        if entry.admin.join(operation).exists() {
            return Err(refuse(&format!(
                "another Git operation is in progress ({operation})"
            )));
        }
    }
    let orig_head = read_pseudo_ref(&entry.path, "ORIG_HEAD").map_err(|reason| refuse(&reason))?;
    if orig_head.as_deref() != Some(format!("{task_tip}\n").as_bytes()) {
        return Err(refuse("ORIG_HEAD does not match the task tip"));
    }
    ensure_no_merge_autostash(&entry.path).map_err(|reason| refuse(&reason))?;
    let merge_head = read_pseudo_ref(&entry.path, "MERGE_HEAD")
        .map_err(|reason| refuse(&reason))?
        .ok_or_else(|| refuse("MERGE_HEAD is missing"))?;
    let merge_head_text =
        std::str::from_utf8(&merge_head).map_err(|_| refuse("MERGE_HEAD is not valid UTF-8"))?;
    let merge_head = merge_head_text
        .strip_suffix('\n')
        .unwrap_or(merge_head_text);
    if !git::is_oid(merge_head) {
        return Err(refuse("MERGE_HEAD is not a valid commit ID"));
    }
    if !is_ancestor(&repo.root, task_base, task_tip)?
        || !is_ancestor(&repo.root, task_base, merge_head)?
    {
        return Err(refuse("task and recorded merge do not share the task base"));
    }
    if !is_ancestor(&repo.root, merge_head, main_tip)? {
        return Err(refuse("MERGE_HEAD is not an ancestor of current main"));
    }
    ensure_ref(&repo.root, main_ref, main_tip)?;
    ensure_visible_index_flags(&entry.path).map_err(|reason| refuse(&reason))?;
    let unmerged = git::output(&entry.path, ["ls-files", "--unmerged"])?;
    if !unmerged.status.success() || !unmerged.stdout.is_empty() {
        return Err(refuse("the merge index contains unresolved entries"));
    }

    let preview = git::output(
        &repo.root,
        [
            "merge-tree",
            "--write-tree",
            "--no-messages",
            task_tip,
            merge_head,
        ],
    )?;
    if preview.status.code() != Some(0) {
        return Err(refuse(
            "the recorded merge no longer has a clean merge tree",
        ));
    }
    let expected_tree = String::from_utf8_lossy(&preview.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !git::is_oid(&expected_tree) {
        return Err(refuse("merge-tree returned no valid result tree"));
    }
    let task_tree = format!("{task_tip}^{{tree}}");
    let restore_paths = verify_abort_restore_paths(&entry.path, task_tip, &expected_tree, &[])
        .map_err(|reason| refuse(&reason))?;
    verify_filter_paths(
        repo,
        &entry.path,
        &restore_paths,
        &task_tree,
        allow_annex_filter,
        "task-tip paths restored by abort",
    )
    .map_err(|error| refuse(&error.msg))?;
    verify_tracked_filter_safety(repo, &entry.path, &expected_tree, allow_annex_filter)
        .map_err(|error| refuse(&error.msg))?;
    preflight::check_merge_paths(&entry.path, task_tip, &expected_tree)
        .map_err(|reason| refuse(&reason))?;
    if !index_matches_tree(&entry.path, &expected_tree).map_err(|reason| refuse(&reason))? {
        return Err(refuse(
            "the merge index differs from its expected merge tree",
        ));
    }
    if !tracked_worktree_matches_index(repo, &entry.path, &expected_tree, allow_annex_filter)
        .map_err(|reason| refuse(&reason))?
    {
        return Err(refuse(
            "tracked worktree files differ from the merge index after verified filters",
        ));
    }

    let extras_before = extra_status_entries(&entry.path).map_err(|reason| refuse(&reason))?;
    verify_abort_restore_paths(&entry.path, task_tip, &expected_tree, &extras_before)
        .map_err(|reason| refuse(&reason))?;
    let refs_before = git::run(
        &repo.root,
        ["for-each-ref", "--format=%(refname) %(objectname)"],
    )?;
    ensure_ref(&repo.root, main_ref, main_tip)?;
    ensure_no_cwd_users(entry, "failed-merge recovery")?;
    let no_artifact_plan = artifact::Plan::default();
    let open_users = worktree_open_path_users(entry, &extras_before, &no_artifact_plan)
        .map_err(|reason| refuse(&reason))?;
    if !open_users.is_empty() {
        return Err(refuse(&format!(
            "processes have open handles in the task worktree (pids: {})",
            open_users.join(", ")
        )));
    }
    if extra_status_entries(&entry.path).map_err(|reason| refuse(&reason))? != extras_before {
        return Err(refuse(
            "untracked or ignored paths changed during recovery checks",
        ));
    }
    verify_abort_restore_paths(&entry.path, task_tip, &expected_tree, &extras_before)
        .map_err(|reason| refuse(&reason))?;
    ensure_no_merge_autostash(&entry.path).map_err(|reason| refuse(&reason))?;
    ensure_ref(&repo.root, main_ref, main_tip)?;
    let refs_now = git::run(
        &repo.root,
        ["for-each-ref", "--format=%(refname) %(objectname)"],
    )?;
    if refs_now != refs_before {
        return Err(refuse("Git refs changed during recovery checks"));
    }
    ensure_no_cwd_users(entry, "failed-merge recovery abort")?;

    let abort = git::output(&entry.path, ["merge", "--abort"])?;
    if !abort.status.success() {
        return Err(Fail::new(
            4,
            format!(
                "{id} recovery abort failed; inspect the current worktree state: {}",
                git::stderr_of(&abort)
            ),
        ));
    }
    let restored_head = git::run(&entry.path, ["rev-parse", "HEAD"])?;
    if restored_head.trim() != task_tip {
        return Err(Fail::new(
            4,
            format!("{id} abort completed but task HEAD differs from its original tip"),
        ));
    }
    ensure_no_merge_autostash(&entry.path)
        .map_err(|reason| Fail::new(4, format!("{id} abort completed but {reason}")))?;
    ensure_pseudo_ref_absent(&entry.path, "MERGE_HEAD")
        .map_err(|reason| Fail::new(4, format!("{id} abort completed but {reason}")))?;
    if !index_matches_tree(&entry.path, &task_tree)
        .map_err(|reason| Fail::new(4, format!("{id} abort completed but {reason}")))?
        || !tracked_worktree_matches_index(repo, &entry.path, &task_tree, allow_annex_filter)
            .map_err(|reason| Fail::new(4, format!("{id} abort completed but {reason}")))?
    {
        return Err(Fail::new(
            4,
            format!("{id} abort completed but tracked state differs from the original task tip"),
        ));
    }
    let current_main = git::run(&repo.root, ["rev-parse", "--verify", main_ref])?;
    if current_main.trim() != main_tip {
        return Err(Fail::new(
            4,
            format!("{id} abort completed but main moved during recovery"),
        ));
    }
    let refs_after = git::run(
        &repo.root,
        ["for-each-ref", "--format=%(refname) %(objectname)"],
    )?;
    if refs_after != refs_before {
        return Err(Fail::new(
            4,
            format!("{id} abort completed but Git refs changed during recovery"),
        ));
    }
    if extra_status_entries(&entry.path)
        .map_err(|reason| Fail::new(4, format!("{id} abort completed but {reason}")))?
        != extras_before
    {
        return Err(Fail::new(
            4,
            format!("{id} abort completed but untracked or ignored paths changed"),
        ));
    }
    Ok(())
}

fn tree_entries(worktree: &Path, treeish: &str) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, String> {
    let output = git::output(worktree, ["ls-tree", "-r", "-z", "--full-tree", treeish])
        .map_err(|error| error.msg)?;
    if !output.status.success() {
        return Err(format!("git ls-tree failed: {}", git::stderr_of(&output)));
    }
    let mut entries = BTreeMap::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|r| !r.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "git ls-tree returned a malformed entry".to_string())?;
        entries.insert(record[tab + 1..].to_vec(), record[..tab].to_vec());
    }
    Ok(entries)
}

fn trim_trailing_slashes(mut path: &[u8]) -> &[u8] {
    while path.last() == Some(&b'/') {
        path = &path[..path.len() - 1];
    }
    path
}

fn paths_overlap(left: &[u8], right: &[u8]) -> bool {
    let left = trim_trailing_slashes(left);
    let right = trim_trailing_slashes(right);
    let component_prefix = |prefix: &[u8], path: &[u8]| {
        path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.get(prefix.len()) == Some(&b'/')
    };
    left == right || component_prefix(left, right) || component_prefix(right, left)
}

fn verify_abort_restore_paths(
    worktree: &Path,
    task_tip: &str,
    merge_tree: &str,
    extras: &[git::StatusEntry],
) -> Result<Vec<Vec<u8>>, String> {
    let task_entries = tree_entries(worktree, task_tip)?;
    let merge_entries = tree_entries(worktree, merge_tree)?;
    let restored: Vec<_> = task_entries
        .iter()
        .filter(|(path, entry)| merge_entries.get(*path) != Some(*entry))
        .map(|(path, _)| path.clone())
        .collect();
    for extra in extras {
        if let Some(path) = restored
            .iter()
            .find(|tracked| paths_overlap(&extra.path, tracked))
        {
            return Err(format!(
                "untracked or ignored path {:?} collides with path {:?} restored by abort",
                String::from_utf8_lossy(&extra.path),
                String::from_utf8_lossy(path)
            ));
        }
    }
    Ok(restored)
}

fn worktree_open_path_users(
    entry: &Entry,
    extras: &[git::StatusEntry],
    plan: &artifact::Plan,
) -> Result<Vec<String>, String> {
    let tracked = git::output(&entry.path, ["ls-files", "-z"]).map_err(|error| error.msg)?;
    if !tracked.status.success() {
        return Err(format!("git ls-files failed: {}", git::stderr_of(&tracked)));
    }
    let mut paths = BTreeSet::from([entry.path.clone()]);
    for path in tracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        paths.insert(entry.path.join(OsString::from_vec(path.to_vec())));
    }
    let mut expanded_extra_paths = 0usize;
    for extra in extras {
        let relative = Path::new(OsStr::from_bytes(&extra.path));
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err("untracked or ignored path is outside the worktree".into());
        }
        collect_open_paths(
            &entry.path.join(OsString::from_vec(extra.path.clone())),
            &mut paths,
            &mut expanded_extra_paths,
        )?;
    }
    for artifact in plan.generated.iter().chain(plan.direnv.iter()) {
        for entry in &artifact.signature {
            paths.insert(if entry.relative == "." {
                artifact.path.clone()
            } else {
                artifact.path.join(&entry.relative)
            });
        }
    }
    artifact::open_path_users(&paths.into_iter().collect::<Vec<_>>())
}

const MAX_EXTRA_OPEN_PATHS: usize = 100_000;

fn collect_open_paths(
    path: &Path,
    paths: &mut BTreeSet<PathBuf>,
    count: &mut usize,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect untracked or ignored path {}: {error}",
            path.display()
        )
    })?;
    *count = count
        .checked_add(1)
        .ok_or_else(|| "too many untracked or ignored paths to inspect".to_string())?;
    if *count > MAX_EXTRA_OPEN_PATHS {
        return Err("too many untracked or ignored paths to inspect".into());
    }
    // lsof follows symlinks; probing generated or broken targets is outside this worktree check.
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if !paths.insert(path.to_path_buf()) {
        return Ok(());
    }
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    for child in fs::read_dir(path).map_err(|error| {
        format!(
            "cannot read untracked or ignored directory {}: {error}",
            path.display()
        )
    })? {
        let child = child.map_err(|error| {
            format!(
                "cannot enumerate untracked or ignored directory {}: {error}",
                path.display()
            )
        })?;
        collect_open_paths(&child.path(), paths, count)?;
    }
    Ok(())
}

fn extra_status_entries(worktree: &Path) -> Result<Vec<git::StatusEntry>, String> {
    let status = git::output(worktree, git::STATUS_ARGS).map_err(|error| error.msg)?;
    if !status.status.success() {
        return Err(format!("git status failed: {}", git::stderr_of(&status)));
    }
    Ok(git::parse_status_entries(&status.stdout)
        .map_err(|why| format!("git status returned malformed data: {why}"))?
        .into_iter()
        .filter(|entry| entry.code == *b"??" || entry.code == *b"!!")
        .collect())
}

fn ensure_no_merge_autostash(worktree: &Path) -> Result<(), String> {
    ensure_pseudo_ref_absent(worktree, "MERGE_AUTOSTASH")
}

fn ensure_pseudo_ref_absent(worktree: &Path, name: &str) -> Result<(), String> {
    if read_pseudo_ref(worktree, name)?.is_some() {
        return Err(format!("{name} is present"));
    }
    Ok(())
}

fn read_pseudo_ref(worktree: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    let raw = git::run(worktree, ["rev-parse", "--git-path", name]).map_err(|error| error.msg)?;
    let path = PathBuf::from(raw.trim());
    let path = if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    };
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot open {name}: {error}")),
    };
    if !file
        .metadata()
        .map_err(|error| format!("cannot inspect {name}: {error}"))?
        .is_file()
    {
        return Err(format!("{name} is not a regular file"));
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|error| format!("cannot read {name}: {error}"))?;
    Ok(Some(contents))
}

fn ensure_visible_index_flags(worktree: &Path) -> Result<(), String> {
    let flags = git::output(worktree, ["ls-files", "-v", "-z"]).map_err(|error| error.msg)?;
    if !flags.status.success() {
        return Err(format!("git ls-files failed: {}", git::stderr_of(&flags)));
    }
    if git::count_hidden(&flags.stdout) > 0 {
        return Err("index contains assume-unchanged or skip-worktree entries".into());
    }
    Ok(())
}

fn index_matches_tree(worktree: &Path, tree: &str) -> Result<bool, String> {
    let diff = git::output(
        worktree,
        [
            "diff",
            "--cached",
            "--quiet",
            "--ignore-submodules=none",
            tree,
            "--",
        ],
    )
    .map_err(|error| error.msg)?;
    match diff.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git diff --cached failed: {}",
            git::stderr_of(&diff)
        )),
    }
}

const FILTER_SCAN_BATCH_PATHS: usize = 256;
const FILTER_SCAN_MAX_PATHS: usize = 100_000;
const FILTER_SCAN_MAX_PATH_BYTES: usize = 16 * 1024 * 1024;
const FILTER_SCAN_MAX_INDEX_BYTES: usize = 64 * 1024 * 1024;

fn worktree_filter_attributes(
    worktree: &Path,
    source_tree: Option<&str>,
    paths: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![OsString::from("check-attr")];
    if let Some(tree) = source_tree {
        args.push(format!("--source={tree}").into());
    }
    args.extend(["-z".into(), "filter".into(), "--stdin".into()]);
    let mut command = git::command(worktree, args);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run git check-attr: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "git check-attr stdin was unavailable".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "git check-attr stdout was unavailable".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "git check-attr stderr was unavailable".to_string())?;
    let writer_paths = paths.to_vec();
    let writer = std::thread::Builder::new()
        .name("wtctl-check-attr-stdin".into())
        .spawn(move || -> Result<(), String> {
            for path in &writer_paths {
                stdin
                    .write_all(path)
                    .map_err(|error| format!("cannot write filter path: {error}"))?;
                stdin
                    .write_all(b"\0")
                    .map_err(|error| format!("cannot write filter path terminator: {error}"))?;
            }
            Ok(())
        })
        .map_err(|error| format!("cannot start git check-attr writer: {error}"))?;
    let stdout_reader = std::thread::Builder::new()
        .name("wtctl-check-attr-stdout".into())
        .spawn(move || -> Result<Vec<u8>, String> {
            let mut output = Vec::new();
            stdout
                .read_to_end(&mut output)
                .map_err(|error| format!("cannot read git check-attr stdout: {error}"))?;
            Ok(output)
        })
        .map_err(|error| format!("cannot start git check-attr stdout reader: {error}"))?;
    let stderr_reader = std::thread::Builder::new()
        .name("wtctl-check-attr-stderr".into())
        .spawn(move || -> Result<Vec<u8>, String> {
            let mut output = Vec::new();
            stderr
                .read_to_end(&mut output)
                .map_err(|error| format!("cannot read git check-attr stderr: {error}"))?;
            Ok(output)
        })
        .map_err(|error| format!("cannot start git check-attr stderr reader: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for git check-attr: {error}"))?;
    let write_result = writer
        .join()
        .map_err(|_| "git check-attr stdin writer panicked".to_string())?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| "git check-attr stdout reader panicked".to_string())??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "git check-attr stderr reader panicked".to_string())??;
    if !status.success() {
        return Err(format!(
            "git check-attr failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    write_result?;
    let mut fields: Vec<_> = stdout.split(|byte| *byte == 0).collect();
    if fields.last() == Some(&&b""[..]) {
        fields.pop();
    }
    if fields.len() != paths.len() * 3 {
        return Err("git check-attr returned malformed filter data".into());
    }
    let mut values = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let fields = &fields[index * 3..index * 3 + 3];
        if fields[0] != path.as_slice() || fields[1] != b"filter" {
            return Err("git check-attr returned malformed filter data".into());
        }
        values.push(fields[2].to_vec());
    }
    Ok(values)
}

fn worktree_filter_attribute(
    worktree: &Path,
    source_tree: Option<&str>,
    relative_path: &Path,
) -> Result<Vec<u8>, String> {
    worktree_filter_attributes(
        worktree,
        source_tree,
        &[relative_path.as_os_str().as_bytes().to_vec()],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| "git check-attr returned no filter value".into())
}

fn annex_filter_matches_path(
    repo: &Repo,
    worktree: &Path,
    expected_tree: &str,
    relative_path: &Path,
    allow_annex_filter: bool,
    annex_filter_verified: &mut bool,
) -> Result<bool, String> {
    if !allow_annex_filter {
        return Ok(false);
    }
    let current = worktree_filter_attribute(worktree, None, relative_path)?;
    let expected = worktree_filter_attribute(worktree, Some(expected_tree), relative_path)?;
    if current.as_slice() != b"annex" || expected.as_slice() != b"annex" {
        return Ok(false);
    }
    if !*annex_filter_verified {
        verify_annex_filter_config(repo, worktree).map_err(|error| error.msg)?;
        *annex_filter_verified = true;
    }
    Ok(true)
}

fn hash_worktree_path_with_filters(
    worktree: &Path,
    relative_path: &Path,
) -> Result<String, String> {
    let mut path_arg = OsString::from("--path=");
    path_arg.push(relative_path.as_os_str());
    let output = git::output(
        worktree,
        [
            OsStr::new("hash-object"),
            OsStr::new("--filters"),
            path_arg.as_os_str(),
            OsStr::new("--"),
            relative_path.as_os_str(),
        ],
    )
    .map_err(|error| error.msg)?;
    if !output.status.success() {
        return Err(format!(
            "git hash-object with verified Git filters failed: {}",
            git::stderr_of(&output)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn tracked_worktree_matches_index(
    repo: &Repo,
    worktree: &Path,
    expected_tree: &str,
    allow_annex_filter: bool,
) -> Result<bool, String> {
    let index = git::output(worktree, ["ls-files", "--stage", "-z"]).map_err(|error| error.msg)?;
    if !index.status.success() {
        return Err(format!("git ls-files failed: {}", git::stderr_of(&index)));
    }
    let mut annex_filter_verified = false;
    for entry in index
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "git ls-files returned a malformed index entry".to_string())?;
        let fields: Vec<_> = entry[..tab].split(|byte| *byte == b' ').collect();
        if fields.len() != 3 || fields[2] != b"0" {
            return Err("index contains a non-stage-0 or malformed entry".into());
        }
        let mode = fields[0];
        let oid = std::str::from_utf8(fields[1])
            .map_err(|_| "git ls-files returned a non-UTF-8 object ID".to_string())?;
        let relative = OsString::from_vec(entry[tab + 1..].to_vec());
        let relative_path = Path::new(&relative);
        if relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err("index contains a path outside the worktree".into());
        }
        if !tracked_parent_dirs_are_real(worktree, relative_path)? {
            return Ok(false);
        }
        let path = worktree.join(&relative);
        match mode {
            b"100644" | b"100755" => {
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(format!("cannot inspect tracked file: {error}")),
                };
                if !metadata.file_type().is_file() {
                    return Ok(false);
                }
                let executable = metadata.mode() & 0o111 != 0;
                if executable != (mode == b"100755") {
                    return Ok(false);
                }
                let hash = git::output(
                    worktree,
                    [
                        OsStr::new("hash-object"),
                        OsStr::new("--no-filters"),
                        path.as_os_str(),
                    ],
                )
                .map_err(|error| error.msg)?;
                if !hash.status.success() {
                    return Err(format!(
                        "cannot hash tracked file {}: {}",
                        path.display(),
                        git::stderr_of(&hash)
                    ));
                }
                if String::from_utf8_lossy(&hash.stdout).trim() != oid {
                    let annex_filter = annex_filter_matches_path(
                        repo,
                        worktree,
                        expected_tree,
                        relative_path,
                        allow_annex_filter,
                        &mut annex_filter_verified,
                    )?;
                    if !annex_filter
                        || hash_worktree_path_with_filters(worktree, relative_path)? != oid
                    {
                        return Ok(false);
                    }
                }
            }
            b"120000" => {
                let target = match fs::read_link(&path) {
                    Ok(target) => target,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(format!("cannot read tracked symlink: {error}")),
                };
                let hash = hash_blob_bytes(worktree, target.as_os_str().as_bytes())?;
                if hash != oid {
                    return Ok(false);
                }
            }
            b"160000" => {
                return Err(
                    "automatic recovery is disabled for worktrees containing gitlinks".into(),
                )
            }
            _ => {
                return Err(format!(
                    "unsupported tracked mode {}",
                    String::from_utf8_lossy(mode)
                ))
            }
        }
    }
    Ok(true)
}

fn tracked_parent_dirs_are_real(worktree: &Path, relative: &Path) -> Result<bool, String> {
    let components: Vec<_> = relative.components().collect();
    if components.is_empty() {
        return Ok(false);
    }
    let mut path = worktree.to_path_buf();
    for component in &components[..components.len() - 1] {
        let Component::Normal(name) = component else {
            return Err("tracked path contains a non-normal parent component".into());
        };
        path.push(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "cannot inspect tracked parent {}: {error}",
                    path.display()
                ))
            }
        };
        if !metadata.file_type().is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn hash_blob_bytes(worktree: &Path, bytes: &[u8]) -> Result<String, String> {
    let mut command = git::command(worktree, ["hash-object", "--stdin"]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start git hash-object: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "git hash-object stdin was unavailable".to_string())?
        .write_all(bytes)
        .map_err(|error| format!("cannot send symlink content to git hash-object: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for git hash-object: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn merge_state_diagnostic(entry: &Entry) -> String {
    let read = |args: &[&str]| match git::output(&entry.path, args) {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        Ok(output) => format!("error: {}", git::stderr_of(&output)),
        Err(error) => format!("error: {}", error.msg),
    };
    format!(
        "current HEAD={}, MERGE_HEAD={}, status={:?}",
        read(&["rev-parse", "HEAD"]),
        read(&["rev-parse", "--verify", "MERGE_HEAD"]),
        read(git::STATUS_ARGS)
    )
}

fn check_active_filters(
    repo: &Repo,
    worktree: &Path,
    old: &str,
    tree: &str,
    allow_annex_filter: bool,
) -> R<()> {
    let changed = git::output(
        &repo.root,
        ["diff", "--name-only", "-z", "--no-renames", old, tree],
    )?;
    if !changed.status.success() {
        return refuse(format!(
            "cannot inspect changed paths for filters: {}",
            git::stderr_of(&changed)
        ));
    }
    if changed.stdout.len() > FILTER_SCAN_MAX_INDEX_BYTES {
        return refuse("changed paths exceed the filter-scan size limit");
    }
    let paths: Vec<Vec<u8>> = changed
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    if paths.len() > FILTER_SCAN_MAX_PATHS
        || paths.iter().map(Vec::len).sum::<usize>() > FILTER_SCAN_MAX_PATH_BYTES
    {
        return refuse("changed paths exceed the filter-scan path limit");
    }
    let mut active = Vec::new();
    let mut annex = Vec::new();
    for batch in paths.chunks(FILTER_SCAN_BATCH_PATHS) {
        let current = worktree_filter_attributes(worktree, None, batch)
            .map_err(|reason| Fail::new(1, reason))?;
        let expected = worktree_filter_attributes(worktree, Some(tree), batch)
            .map_err(|reason| Fail::new(1, reason))?;
        for ((path, current), expected) in batch.iter().zip(current).zip(expected) {
            let current_active = !matches!(current.as_slice(), b"unspecified" | b"unset");
            let expected_active = !matches!(expected.as_slice(), b"unspecified" | b"unset");
            if !current_active && !expected_active {
                continue;
            }
            let description = format!(
                "{}: current filter={}, expected filter={}",
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(&current),
                String::from_utf8_lossy(&expected)
            );
            if current.as_slice() == b"annex" && expected.as_slice() == b"annex" {
                annex.push(description);
            } else {
                active.push(description);
            }
        }
    }
    if !active.is_empty() {
        return refuse(format!(
            "changed paths use unreviewed Git filters: {}",
            active.join(", ")
        ));
    }
    if !annex.is_empty() {
        if !allow_annex_filter {
            return refuse(format!(
                "changed paths use Git-annex filters; inspect and pass --allow-annex-filter: {}",
                annex.join(", ")
            ));
        }
        verify_annex_filter_config(repo, worktree)?;
    }
    Ok(())
}

fn indexed_filter_paths(worktree: &Path) -> Result<Vec<Vec<u8>>, String> {
    let index = git::output(worktree, ["ls-files", "--stage", "-z"]).map_err(|error| error.msg)?;
    if !index.status.success() {
        return Err(format!("git ls-files failed: {}", git::stderr_of(&index)));
    }
    if index.stdout.len() > FILTER_SCAN_MAX_INDEX_BYTES {
        return Err("index exceeds the filter-scan size limit".into());
    }
    let mut paths = Vec::new();
    let mut path_bytes = 0usize;
    for entry in index
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| "git ls-files returned a malformed index entry".to_string())?;
        let fields: Vec<_> = entry[..tab].split(|byte| *byte == b' ').collect();
        if fields.len() != 3 || fields[2] != b"0" {
            return Err("index contains a non-stage-0 or malformed entry".into());
        }
        let path = entry[tab + 1..].to_vec();
        let relative = Path::new(OsStr::from_bytes(&path));
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err("index contains a path outside the worktree".into());
        }
        path_bytes = path_bytes
            .checked_add(path.len())
            .ok_or_else(|| "index paths exceed the filter-scan size limit".to_string())?;
        if paths.len() >= FILTER_SCAN_MAX_PATHS || path_bytes > FILTER_SCAN_MAX_PATH_BYTES {
            return Err("index paths exceed the filter-scan path limit".into());
        }
        paths.push(path);
    }
    Ok(paths)
}

fn verify_tracked_filter_safety(
    repo: &Repo,
    worktree: &Path,
    task_tree: &str,
    allow_annex_filter: bool,
) -> R<()> {
    let paths = indexed_filter_paths(worktree).map_err(|reason| Fail::new(4, reason))?;
    verify_filter_paths(
        repo,
        worktree,
        &paths,
        task_tree,
        allow_annex_filter,
        "tracked paths",
    )
}

fn verify_filter_paths(
    repo: &Repo,
    worktree: &Path,
    paths: &[Vec<u8>],
    expected_tree: &str,
    allow_annex_filter: bool,
    scope: &str,
) -> R<()> {
    let path_bytes = paths
        .iter()
        .try_fold(0usize, |total, path| total.checked_add(path.len()));
    if paths.len() > FILTER_SCAN_MAX_PATHS
        || path_bytes.is_none_or(|total| total > FILTER_SCAN_MAX_PATH_BYTES)
    {
        return refuse(format!("{scope} exceed the filter-scan path limit"));
    }
    let mut annex = false;
    for batch in paths.chunks(FILTER_SCAN_BATCH_PATHS) {
        let current = worktree_filter_attributes(worktree, None, batch)
            .map_err(|reason| Fail::new(1, reason))?;
        let expected = worktree_filter_attributes(worktree, Some(expected_tree), batch)
            .map_err(|reason| Fail::new(1, reason))?;
        for ((path, current), expected) in batch.iter().zip(current).zip(expected) {
            let current_active = !matches!(current.as_slice(), b"unspecified" | b"unset");
            let expected_active = !matches!(expected.as_slice(), b"unspecified" | b"unset");
            if !current_active && !expected_active {
                continue;
            }
            if current.as_slice() != b"annex" || expected.as_slice() != b"annex" {
                return refuse(format!(
                    "{scope} use an unreviewed or mismatched filter: {} (current={}, expected tree={})",
                    String::from_utf8_lossy(path),
                    String::from_utf8_lossy(&current),
                    String::from_utf8_lossy(&expected)
                ));
            }
            annex = true;
        }
    }
    if annex {
        if !allow_annex_filter {
            return refuse(format!(
                "{scope} use Git-annex filters; pass --allow-annex-filter"
            ));
        }
        verify_annex_filter_config(repo, worktree)?;
    }
    Ok(())
}

fn verify_annex_filter_config(repo: &Repo, worktree: &Path) -> R<()> {
    let expected = [
        ("filter.annex.clean", "git-annex smudge --clean -- %f"),
        ("filter.annex.smudge", "git-annex smudge -- %f"),
        ("filter.annex.process", "git-annex filter-process"),
    ];
    let mut mismatches = Vec::new();
    for (key, expected) in expected {
        let config = git::output(worktree, ["config", "--get", key])?;
        if !config.status.success() || String::from_utf8_lossy(&config.stdout).trim() != expected {
            mismatches.push(key);
        }
    }
    if !mismatches.is_empty() || !repo.require_annex_ready(true)? {
        return refuse(format!(
            "git-annex filter configuration is not verified: {}",
            mismatches.join(", ")
        ));
    }
    Ok(())
}

fn check_commit_signing(repo: &Repo, allow_commit_signing: bool) -> R<()> {
    let signing = git::output(&repo.root, ["config", "--bool", "--get", "commit.gpgsign"])?;
    if !signing.status.success() && signing.status.code() != Some(1) {
        return refuse(format!(
            "cannot inspect commit signing configuration: {}",
            git::stderr_of(&signing)
        ));
    }
    if signing.status.success()
        && String::from_utf8_lossy(&signing.stdout).trim() == "true"
        && !allow_commit_signing
    {
        return refuse(
            "integration merge is configured to sign; authorize it with --allow-commit-signing",
        );
    }
    Ok(())
}

fn cmd_finish(a: &Args) -> R<()> {
    if a.flag("--dry-run") {
        return finish_plan::cmd_finish_preflight(a);
    }
    let repo = a.repo(true)?;
    let strategy = strategy::parse(a)?;
    strategy::validate(a, strategy)?;
    let id = a.get("--id")?;
    let legacy_id = a.get("--legacy-worktree-id")?;
    let branch = a.get("--expect-branch")?;
    let recover_failed_merge = a.flag("--recover-failed-merge");
    if [id, legacy_id, branch]
        .into_iter()
        .filter(Option::is_some)
        .count()
        != 1
    {
        return Err(Fail::new(
            2,
            "provide exactly one of --id, --legacy-worktree-id, or --expect-branch",
        ));
    }
    if a.flag("--delete-branch") && branch.is_none() {
        return Err(Fail::new(2, "--delete-branch requires --expect-branch"));
    }
    let keep_worktree = a.flag("--keep-worktree");
    let integration_message_arg = a.get("--integration-message")?;
    if recover_failed_merge && id.is_none() {
        return Err(Fail::new(2, "--recover-failed-merge requires --id"));
    }
    if recover_failed_merge && integration_message_arg.is_none() {
        return Err(Fail::new(
            2,
            "--recover-failed-merge requires an explicit --integration-message",
        ));
    }
    let integration_message = integration_message_arg.unwrap_or("chore: integrate task worktree");
    if integration_message.trim().is_empty()
        || integration_message.contains('\n')
        || integration_message.contains('\r')
    {
        return Err(Fail::new(
            2,
            "--integration-message must be a non-empty single line",
        ));
    }
    if keep_worktree
        && (a.flag("--delete-branch")
            || a.flag("--discard-direnv-cache")
            || a.flag("--discard-generated-caches")
            || a.flag("--discard-generated-build-outputs")
            || a.flag("--discard-generated-artifacts")
            || a.flag("--preserve-generated-artifacts"))
    {
        return Err(Fail::new(
            2,
            "--keep-worktree cannot be combined with branch or artifact cleanup options",
        ));
    }
    if !a.flag("--external-writers-quiesced") {
        return refuse(
            "inspect scheduled and non-cooperating writers, then pass --external-writers-quiesced",
        );
    }
    let mut cleanup = if keep_worktree {
        artifact::Options::default()
    } else {
        artifact_options(a, true)?
    };
    let (main, refname) = main_ref(a, &repo.root)?;
    let annex = repo.require_annex_ready(true)?;
    let _lock = repo.lock()?;
    let (mut entry, task_base, marker, removal, display_id, caller) = if let Some(id) = id {
        let (entry, marker) = repo.find_task(id)?;
        let caller = require_owner(&marker)?;
        let removal = RemovalAuthority {
            worktree_id: entry.id(),
            wtctl_task_id: Some(marker.task_id.clone()),
            expected_marker: Some(marker.raw.clone()),
            expected_lock_reason: Some(format!("agent-task {id}")),
            legacy_owner: None,
            expected_branch: None,
        };
        (
            entry,
            marker.base.clone(),
            Some(marker),
            removal,
            id.to_string(),
            caller,
        )
    } else {
        let (entry, trailer) = find_legacy_target(&repo, legacy_id, branch)?;
        if !legacy_is_mine(&trailer) {
            return refuse("legacy worktree belongs to another or unknown client session");
        }
        let task_base = trailer
            .get("base")
            .filter(|base| git::is_oid(base))
            .cloned()
            .ok_or_else(|| Fail::new(4, "legacy owner metadata has no valid base OID"))?;
        let client = trailer["client"].clone();
        let session = trailer["session"].clone();
        let display_id = branch
            .map(str::to_string)
            .or_else(|| legacy_id.map(str::to_string))
            .unwrap_or_else(|| entry.id());
        let removal = RemovalAuthority {
            worktree_id: entry.id(),
            wtctl_task_id: None,
            expected_marker: None,
            expected_lock_reason: None,
            legacy_owner: Some((client, session)),
            expected_branch: branch.map(str::to_string),
        };
        (entry, task_base, None, removal, display_id, me()?)
    };
    let mut main_tip = git::run(&repo.root, ["rev-parse", "--verify", &refname])?
        .trim()
        .to_string();
    let main_before = main_tip.clone();
    let task_tip = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, format!("cannot resolve {display_id} HEAD")))?;
    if recover_failed_merge {
        let external_writers_quiesced = a.flag("--external-writers-quiesced");
        verify_removal_authority(&repo, &entry, &removal)?;
        if let Some(marker) = &marker {
            require_owner(marker)?;
        }
        recover_pending_merge(
            &repo,
            &display_id,
            &entry,
            &refname,
            &main_tip,
            &task_base,
            &task_tip,
            a.flag("--allow-annex-filter"),
            external_writers_quiesced,
        )?;
        entry = Entry::load(entry.admin.clone()).ok_or_else(|| {
            Fail::new(
                4,
                format!("{display_id} registration disappeared after merge recovery"),
            )
        })?;
        verify_removal_authority(&repo, &entry, &removal)?;
        if let Some(marker) = &marker {
            require_owner(marker)?;
        }
        if entry.current_oid().as_deref() != Some(task_tip.as_str()) {
            return refuse(format!(
                "{display_id} HEAD changed after failed-merge recovery"
            ));
        }
        main_tip = git::run(&repo.root, ["rev-parse", "--verify", &refname])?
            .trim()
            .to_string();
    }
    let diverged = !is_ancestor(&repo.root, &main_tip, &task_tip)?
        && !is_ancestor(&repo.root, &task_tip, &main_tip)?;
    if diverged && strategy == strategy::Strategy::Rebase {
        let rebased = strategy::rebase_onto_main(
            &repo,
            &display_id,
            &entry,
            &refname,
            &main_tip,
            &task_tip,
            a.flag("--allow-repository-hooks"),
            a.flag("--allow-commit-signing"),
        )?;
        entry = Entry::load(entry.admin.clone()).ok_or_else(|| {
            Fail::new(
                4,
                format!("{display_id} registration disappeared after rebase"),
            )
        })?;
        if entry.current_oid().as_deref() != Some(rebased.as_str()) {
            return refuse(format!("{display_id} HEAD changed after rebase"));
        }
        verify_removal_authority(&repo, &entry, &removal)?;
        // The pre-rebase commits live only in the reflog, which this opt-in
        // accepts now that HEAD is based on main; the tip is kept in a ref.
        cleanup.allow_unreferenced_reflog = true;
    } else if diverged {
        let merged_tip = merge_main_into_task(
            &repo,
            &display_id,
            &entry,
            &task_base,
            FinishMerge {
                main_ref: &refname,
                main_tip: &main_tip,
                task_tip: &task_tip,
                allow_hooks: a.flag("--allow-repository-hooks"),
                annex,
                cleanup,
                integration_message,
                allow_annex_filter: a.flag("--allow-annex-filter"),
                allow_commit_signing: a.flag("--allow-commit-signing"),
            },
        )?;
        entry = Entry::load(entry.admin.clone()).ok_or_else(|| {
            Fail::new(
                4,
                format!("{display_id} registration disappeared after merge"),
            )
        })?;
        if entry.current_oid().as_deref() != Some(merged_tip.as_str()) {
            return refuse(format!(
                "{display_id} HEAD changed after merge; cleanup stopped"
            ));
        }
        verify_removal_authority(&repo, &entry, &removal)?;
        if let Some(marker) = &marker {
            require_owner(marker)?;
        }
    }
    if strategy == strategy::Strategy::Squash {
        let current = entry
            .current_oid()
            .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD before squash"))?;
        if current != main_tip && !is_ancestor(&repo.root, &current, &main_tip)? {
            let squashed = strategy::squash_onto_main(
                &repo,
                &display_id,
                &entry,
                &main_tip,
                &current,
                integration_message,
                a.flag("--allow-commit-signing"),
            )?;
            entry = Entry::load(entry.admin.clone()).ok_or_else(|| {
                Fail::new(
                    4,
                    format!("{display_id} registration disappeared after squash"),
                )
            })?;
            if entry.current_oid().as_deref() != Some(squashed.as_str()) {
                return refuse(format!("{display_id} HEAD changed after squash"));
            }
            verify_removal_authority(&repo, &entry, &removal)?;
            cleanup.allow_unreferenced_reflog = true;
        }
    }
    let finish_head = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD before integration"))?;
    let (old, tip, already) = integrate_task(
        &repo,
        IntegrationContext {
            id: &display_id,
            main: &main,
            refname: &refname,
            task_base: &task_base,
        },
        &entry,
        &caller,
        IntegrationOptions {
            allow_hooks: a.flag("--allow-repository-hooks"),
            allow_annex_filter: a.flag("--allow-annex-filter"),
            cleanup: Some(cleanup),
            json_output: a.flag("--json"),
        },
    )?;
    let fresh = Entry::load(entry.admin.clone()).ok_or_else(|| {
        Fail::new(
            4,
            format!("{display_id} registration disappeared after integration"),
        )
    })?;
    if fresh.path != entry.path || fresh.current_oid().as_deref() != Some(finish_head.as_str()) {
        return refuse(format!(
            "{display_id} changed after integration; cleanup stopped"
        ));
    }
    verify_removal_authority(&repo, &fresh, &removal)?;
    if let Some(marker) = &marker {
        require_owner(marker)?;
    }
    let post_integration_facts = task::facts(&[&fresh], &refname)?;
    post_integration_facts
        .first()
        .cloned()
        .unwrap_or_default()
        .removable_except_integration()
        .map_err(|why| Fail::new(4, format!("{display_id} changed after integration: {why}")))?;
    let main_worktree = canonical_main_checkout(&repo)?;
    artifact::inspect_plan(&fresh.path, main_worktree.as_deref(), cleanup)
        .map_err(|why| Fail::new(4, format!("{display_id} changed after integration: {why}")))?;
    if keep_worktree {
        repo.audit(json!({
            "at": now(),
            "event": "finish-kept",
            "worktree_id": removal.worktree_id,
            "ref": refname,
            "old": old,
            "new": tip,
            "already_integrated": already,
            "pid": caller.pid,
        }));
        if a.flag("--json") {
            out!(
                "{}",
                json!({
                    "status": "integrated-kept",
                    "repo": repo.root,
                    "main_tip": tip,
                    "task_id": display_id,
                    "worktree_id": removal.worktree_id,
                    "task_tip": finish_head,
                    "main_before": main_before,
                    "worktree_path": fresh.path,
                })
            );
        } else {
            out!("integrated {display_id}: {main} (worktree kept)");
            if already {
                out!("  already integrated; {main} stays at {tip}");
            } else {
                out!("  {main}: {main_before} -> {tip}");
            }
            out!("  kept {}", fresh.path.display());
        }
        return Ok(());
    }
    let finished_head = fresh
        .current_oid()
        .ok_or_else(|| Fail::new(4, "cannot resolve finished worktree HEAD"))?;
    if let Err(fail) = remove_worktree(
        &repo,
        &fresh,
        &removal,
        RemovalContext {
            main_ref: &refname,
            cwds: &procinfo::scan_cwds(),
            annex,
            cleanup,
            json_output: a.flag("--json"),
        },
    ) {
        return Err(Fail::new(
            fail.code,
            format!(
                "{display_id} integrated but cleanup is pending: {}",
                fail.msg
            ),
        ));
    }
    if let Some(branch) = branch {
        if a.flag("--delete-branch") {
            delete_legacy_branch(&repo, branch, &finished_head, &refname)?;
        }
    }
    repo.audit(json!({
        "at": now(),
        "event": "finish",
        "worktree_id": removal.worktree_id,
        "ref": refname,
        "old": old,
        "new": tip,
        "already_integrated": already,
        "pid": caller.pid,
    }));
    if a.flag("--json") {
        out!(
            "{}",
            json!({
                "status": "finished",
                "repo": repo.root,
                "main_tip": tip,
                "task_id": display_id,
                "worktree_id": removal.worktree_id,
                "task_tip": finished_head,
                "already_integrated": already,
                "main_before": main_before,
                "worktree_path": fresh.path,
            })
        );
    } else {
        out!("finished {display_id}: {main}");
        if already {
            out!("  already integrated; {main} stays at {tip}");
        } else {
            out!("  {main}: {main_before} -> {tip}");
        }
        out!("  removed {}", fresh.path.display());
    }
    Ok(())
}

fn gitfile_pointer(entry: &Entry) -> R<Option<OsString>> {
    let dotgit = entry.path.join(".git");
    let admin = canon(&entry.admin);
    let metadata = fs::symlink_metadata(&dotgit)
        .map_err(|e| Fail::new(4, format!("cannot inspect {}: {e}", dotgit.display())))?;
    let symlink_target = if metadata.file_type().is_symlink() {
        let target = fs::read_link(&dotgit)
            .map_err(|e| Fail::new(4, format!("cannot read {} symlink: {e}", dotgit.display())))?;
        if canon(&dotgit) != admin {
            return refuse(format!(
                "{} does not resolve to {}",
                dotgit.display(),
                admin.display()
            ));
        }
        Some(target.into_os_string())
    } else if metadata.is_file() {
        let text = fs::read_to_string(&dotgit)
            .map_err(|e| Fail::new(4, format!("cannot read {} gitfile: {e}", dotgit.display())))?;
        let pointer = text
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| Fail::new(4, format!("{} is not a gitfile", dotgit.display())))?;
        let pointer = PathBuf::from(pointer);
        let pointer = if pointer.is_absolute() {
            pointer
        } else {
            entry.path.join(pointer)
        };
        if canon(&pointer) != admin {
            return refuse(format!(
                "{} does not name {}",
                dotgit.display(),
                admin.display()
            ));
        }
        None
    } else {
        return refuse(format!(
            "{} is neither a gitfile nor a symlink",
            dotgit.display()
        ));
    };

    let back = fs::read_to_string(entry.admin.join("gitdir")).map_err(|e| {
        Fail::new(
            4,
            format!("cannot read {}: {e}", entry.admin.join("gitdir").display()),
        )
    })?;
    let back = PathBuf::from(back.trim());
    let back = if back.is_absolute() {
        back
    } else {
        entry.admin.join(back)
    };
    if back.file_name() != Some(OsStr::new(".git"))
        || back.parent().is_none_or(|p| canon(p) != canon(&entry.path))
    {
        return refuse(format!(
            "{} does not point back to {}",
            entry.admin.join("gitdir").display(),
            dotgit.display()
        ));
    }
    Ok(symlink_target)
}

fn replace_dotgit_with_gitfile(entry: &Entry, expected_link: &OsStr) -> R<()> {
    if gitfile_pointer(entry)?.as_deref() != Some(expected_link) {
        return refuse(format!(
            "{} changed before annex normalization",
            entry.path.join(".git").display()
        ));
    }
    let dotgit = entry.path.join(".git");
    let temporary = entry
        .path
        .join(format!(".git.wtctl-normalize-{}.tmp", random_hex(8)?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temporary)
        .map_err(|e| Fail::new(4, format!("cannot create {}: {e}", temporary.display())))?;
    let result = (|| -> std::io::Result<()> {
        writeln!(file, "gitdir: {}", entry.admin.display())?;
        file.sync_all()?;
        if gitfile_pointer(entry).ok().flatten().as_deref() != Some(expected_link) {
            return Err(std::io::Error::other(
                ".git symlink changed during normalization",
            ));
        }
        fs::rename(&temporary, &dotgit)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return refuse(format!("cannot normalize {}: {error}", dotgit.display()));
    }
    if gitfile_pointer(entry)?.is_some() {
        return refuse(format!(
            "{} remained a symlink after normalization",
            dotgit.display()
        ));
    }
    Ok(())
}

fn restore_dotgit_symlink(entry: &Entry, original: &OsStr) -> R<()> {
    let dotgit = entry.path.join(".git");
    match gitfile_pointer(entry)? {
        Some(current) if current == original => return Ok(()),
        Some(_) => {
            return refuse(format!(
                "{} has a different symlink target",
                dotgit.display()
            ))
        }
        None => {}
    }
    let temporary = entry
        .path
        .join(format!(".git.wtctl-restore-{}.tmp", random_hex(8)?));
    std::os::unix::fs::symlink(original, &temporary).map_err(|e| {
        Fail::new(
            4,
            format!("cannot stage {} symlink restoration: {e}", dotgit.display()),
        )
    })?;
    if gitfile_pointer(entry)?.is_some() {
        let _ = fs::remove_file(&temporary);
        return refuse(format!(
            "{} changed before symlink restoration",
            dotgit.display()
        ));
    }
    fs::rename(&temporary, &dotgit).map_err(|e| {
        Fail::new(
            4,
            format!("cannot restore {} symlink: {e}", dotgit.display()),
        )
    })?;
    if gitfile_pointer(entry)?.as_deref() != Some(original) {
        return refuse(format!(
            "{} symlink restoration did not verify",
            dotgit.display()
        ));
    }
    Ok(())
}

fn annex_restage(
    entry: &Entry,
    expected_head: &str,
    main_worktree: Option<&Path>,
    cleanup: artifact::Options,
    expected_plan: &artifact::Plan,
) -> R<()> {
    let result = git::output(&entry.path, ["annex", "restage"])?;
    if !result.status.success() {
        return refuse(format!(
            "git annex restage failed: {}",
            git::stderr_of(&result)
        ));
    }
    let refreshed = Entry::load(entry.admin.clone()).ok_or_else(|| {
        Fail::new(
            4,
            "worktree registration disappeared after git annex restage",
        )
    })?;
    if refreshed.path != entry.path
        || refreshed.head != expected_head
        || refreshed.id() != entry.id()
    {
        return refuse("worktree identity or HEAD changed during git annex restage");
    }
    let refreshed_plan =
        artifact::inspect_plan(&refreshed.path, main_worktree, cleanup).map_err(|why| {
            Fail::new(
                4,
                format!(
                    "{} changed during git annex restage: {why}",
                    refreshed.path.display()
                ),
            )
        })?;
    if refreshed_plan != *expected_plan {
        return refuse(format!(
            "{} cleanup artifact plan changed during git annex restage",
            refreshed.path.display()
        ));
    }
    Ok(())
}

fn rollback_failed_removal(
    repo: &Repo,
    expected: &Entry,
    authority: &RemovalAuthority,
    expected_head: &str,
    original_symlink: Option<&OsStr>,
    was_locked: bool,
) -> R<()> {
    let fresh = Entry::load(expected.admin.clone()).ok_or_else(|| {
        Fail::new(
            4,
            "Git removal failed after the worktree registration disappeared; inspect the path and admin directory before recovery",
        )
    })?;
    if fresh.path != expected.path
        || fresh.id() != authority.worktree_id
        || fresh.current_oid().as_deref() != Some(expected_head)
        || !fresh.path.is_dir()
    {
        return refuse(
            "Git removal failed after the worktree identity or HEAD changed; preserved remaining state for inspection",
        );
    }
    verify_removal_authority(repo, &fresh, authority)?;
    let pointer = gitfile_pointer(&fresh)?;
    if let Some(original) = original_symlink {
        if pointer
            .as_deref()
            .is_some_and(|current| current != original)
        {
            return refuse(
                "Git removal failed and the .git symlink target changed; left it untouched",
            );
        }
    } else if pointer.is_some() {
        return refuse("Git removal failed and .git changed to a symlink; left it untouched");
    }

    let expected_lock = authority.expected_lock_reason.as_deref();
    match (&fresh.lock_reason, was_locked, expected_lock) {
        (Some(current), true, Some(expected)) if current == expected => {}
        (Some(_), _, _) => {
            return refuse("Git removal failed and the worktree lock changed; left it untouched")
        }
        (None, true, Some(_)) => relock(repo, fresh.path.as_os_str(), &authority.worktree_id)
            .map_err(|e| Fail::new(4, format!("worktree left UNLOCKED ({})", e.msg)))?,
        (None, _, _) => {}
    }
    let refreshed = Entry::load(fresh.admin.clone())
        .ok_or_else(|| Fail::new(4, "worktree registration disappeared during rollback"))?;
    if refreshed.path != expected.path
        || refreshed.id() != authority.worktree_id
        || refreshed.current_oid().as_deref() != Some(expected_head)
        || !refreshed.path.is_dir()
    {
        return refuse(
            "worktree changed while restoring its original state; inspect before recovery",
        );
    }
    verify_removal_authority(repo, &refreshed, authority)?;
    if was_locked && refreshed.lock_reason.as_deref() != expected_lock {
        return refuse("failed to restore the worktree's original lock state");
    }
    if let Some(original) = original_symlink {
        if gitfile_pointer(&refreshed)?.is_none() {
            restore_dotgit_symlink(&refreshed, original)?;
        }
    }
    Ok(())
}

struct RemovalAuthority {
    worktree_id: String,
    wtctl_task_id: Option<String>,
    expected_marker: Option<Value>,
    expected_lock_reason: Option<String>,
    legacy_owner: Option<(String, String)>,
    expected_branch: Option<String>,
}

fn removal_lock_state(entry: &Entry, authority: &RemovalAuthority) -> R<bool> {
    match (
        &entry.lock_reason,
        authority.expected_lock_reason.as_deref(),
    ) {
        (Some(reason), Some(expected)) if reason == expected => Ok(true),
        (Some(reason), _) => refuse(format!(
            "{} is locked by someone else ({reason:?}); leave it alone",
            entry.path.display()
        )),
        (None, _) => Ok(false),
    }
}

fn verify_removal_authority(repo: &Repo, entry: &Entry, authority: &RemovalAuthority) -> R<()> {
    if entry.id() != authority.worktree_id {
        return refuse(format!(
            "expected worktree ID {} but Git registered {}",
            authority.worktree_id,
            entry.id()
        ));
    }
    if let Some(task_id) = &authority.wtctl_task_id {
        if entry.marker().is_none_or(|m| {
            m.task_id != *task_id || authority.expected_marker.as_ref() != Some(&m.raw)
        }) {
            return refuse(format!("wtctl marker mismatch for {task_id}"));
        }
        return Ok(());
    }
    if let Some(branch) = &authority.expected_branch {
        if entry
            .branch_ref()
            .and_then(|r| r.strip_prefix("refs/heads/"))
            != Some(branch.as_str())
        {
            return refuse(format!(
                "expected branch {branch} does not match the registered worktree"
            ));
        }
    } else if entry.branch_ref().is_some() {
        return refuse("legacy branch worktrees require --expect-branch");
    }
    let Some((client, session)) = &authority.legacy_owner else {
        return refuse("legacy worktree removal requires an exact session owner");
    };
    let Some(meta) = legacy_metadata(repo, entry) else {
        return refuse("legacy owner metadata disappeared during removal");
    };
    if meta.get("client") != Some(client) || meta.get("session") != Some(session) {
        return refuse("legacy owner metadata changed during removal");
    }
    if !legacy_is_mine(&meta) {
        return refuse("legacy worktree belongs to another or unknown client session");
    }
    Ok(())
}

struct RemovalContext<'a> {
    main_ref: &'a str,
    cwds: &'a procinfo::CwdScan,
    annex: bool,
    cleanup: artifact::Options,
    json_output: bool,
}

fn remove_worktree(
    repo: &Repo,
    entry: &Entry,
    authority: &RemovalAuthority,
    context: RemovalContext<'_>,
) -> R<()> {
    let RemovalContext {
        main_ref,
        cwds,
        annex,
        cleanup,
        json_output,
    } = context;
    let id = &authority.worktree_id;
    let expected_head = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD before removal"))?;
    artifact::check_pending_transaction(&repo.name(), &entry.path)
        .map_err(|why| Fail::new(4, format!("{id}: {why}")))?;
    if entry.admin.join("modules").exists() {
        return refuse(format!(
            "{id} contains submodule state; remove it separately"
        ));
    }
    let index = git::output(&entry.path, ["ls-files", "--stage", "-z"])?;
    if !index.status.success()
        || index.stdout.split(|byte| *byte == 0).any(|record| {
            record
                .split(|byte| *byte == b'\t')
                .next()
                .is_some_and(|metadata| metadata.starts_with(b"160000 "))
        })
    {
        return refuse(format!(
            "{id} contains submodules or its index cannot be inspected"
        ));
    }
    if entry.admin.join("modules").exists() {
        return refuse(format!(
            "{id} contains submodule state; remove it separately"
        ));
    }
    let index = git::output(&entry.path, ["ls-files", "--stage", "-z"])?;
    if !index.status.success() {
        return refuse(format!("cannot inspect the index of {id} for submodules"));
    }
    if index.stdout.split(|byte| *byte == 0).any(|record| {
        record
            .split(|byte| *byte == b'\t')
            .next()
            .is_some_and(|metadata| metadata.starts_with(b"160000 "))
    }) {
        return refuse(format!("{id} contains submodules; remove them separately"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        if canon(&cwd).starts_with(&entry.path) {
            return refuse(format!("run this outside {}", entry.path.display()));
        }
    }
    let expected_tree = format!("{expected_head}^{{tree}}");
    verify_tracked_filter_safety(repo, &entry.path, &expected_tree, annex)?;
    let facts = task::facts(&[entry], main_ref)?;
    facts
        .first()
        .cloned()
        .unwrap_or_default()
        .removable_except_status_with_reflog_opt_in(cleanup.allow_unreferenced_reflog)
        .map_err(|why| Fail::new(4, format!("{id}: {why}")))?;
    if cleanup.allow_unreferenced_reflog {
        let main_tip = git::run(&repo.root, ["rev-parse", "--verify", main_ref])?;
        if !is_ancestor(&repo.root, &expected_head, main_tip.trim())? {
            return refuse(format!("{id} HEAD is not contained in {main_ref}; reflog opt-in cannot permit an unintegrated task"));
        }
    }
    let users = procinfo::users_inside(cwds, &entry.path).map_err(|e| {
        Fail::new(
            4,
            format!(
                "cannot check processes inside {}: {e}",
                entry.path.display()
            ),
        )
    })?;
    if !users.is_empty() {
        return refuse(format!(
            "processes still have their cwd inside {} (pids: {})",
            entry.path.display(),
            users.join(", ")
        ));
    }
    verify_removal_authority(repo, entry, authority)?;
    let locked = removal_lock_state(entry, authority)?;
    gitfile_pointer(entry)?;
    let main_worktree = canonical_main_checkout(repo)?;
    let plan = artifact::inspect_plan(&entry.path, main_worktree.as_deref(), cleanup)
        .map_err(|why| Fail::new(4, format!("{id}: {why}")))?;
    if annex {
        annex_restage(entry, &entry.head, main_worktree.as_deref(), cleanup, &plan)?;
        let refreshed = Entry::load(entry.admin.clone()).ok_or_else(|| {
            Fail::new(
                4,
                "worktree registration disappeared after git annex restage",
            )
        })?;
        if refreshed.path != entry.path
            || refreshed.head != entry.head
            || refreshed.lock_reason != entry.lock_reason
        {
            return refuse(format!("{id} changed during git annex restage"));
        }
        verify_removal_authority(repo, &refreshed, authority)?;
        let facts = task::facts(&[&refreshed], main_ref)?;
        facts
            .first()
            .cloned()
            .unwrap_or_default()
            .removable_except_status_with_reflog_opt_in(cleanup.allow_unreferenced_reflog)
            .map_err(|why| Fail::new(4, format!("{id} changed during git annex restage: {why}")))?;
        let fresh_cwds = procinfo::scan_cwds();
        let users = procinfo::users_inside(&fresh_cwds, &entry.path).map_err(|e| {
            Fail::new(
                4,
                format!(
                    "cannot re-check processes inside {} after git annex restage: {e}",
                    entry.path.display()
                ),
            )
        })?;
        if !users.is_empty() {
            return refuse(format!(
                "processes entered {} during git annex restage (pids: {})",
                entry.path.display(),
                users.join(", ")
            ));
        }
    }
    let dotgit_symlink = gitfile_pointer(entry)?;
    let mut transaction = artifact::begin_transaction(
        &plan,
        &repo.name(),
        &repo.common,
        &entry.path,
        &entry.admin,
        &expected_head,
        entry.branch_ref(),
    )
    .map_err(|why| Fail::new(4, format!("{id}: {why}")))?;
    let mut rollback_dotgit = dotgit_symlink.clone();

    let operation = (|| -> R<()> {
        if let Some(transaction) = &mut transaction {
            artifact::quarantine(transaction, &plan, &entry.path).map_err(|why| {
                Fail::new(4, format!("cannot quarantine worktree artifacts: {why}"))
            })?;
        }
        let facts = task::facts(&[entry], main_ref)?;
        facts
            .first()
            .cloned()
            .unwrap_or_default()
            .removable_with_reflog_opt_in(cleanup.allow_unreferenced_reflog)
            .map_err(|why| Fail::new(4, format!("{id} changed before removal: {why}")))?;
        verify_removal_authority(repo, entry, authority)?;
        let fresh = Entry::load(entry.admin.clone())
            .ok_or_else(|| Fail::new(4, "worktree registration disappeared before removal"))?;
        if fresh.path != entry.path
            || fresh.current_oid().as_deref() != Some(expected_head.as_str())
        {
            return refuse(format!("{id} identity or HEAD changed before removal"));
        }
        let fresh_cwds = procinfo::scan_cwds();
        let users = procinfo::users_inside(&fresh_cwds, &entry.path).map_err(|error| {
            Fail::new(4, format!("cannot re-check worktree processes: {error}"))
        })?;
        if !users.is_empty() {
            return refuse(format!(
                "processes entered {} before removal (pids: {})",
                entry.path.display(),
                users.join(", ")
            ));
        }
        let pointer = gitfile_pointer(entry)?;
        rollback_dotgit = pointer.clone();
        if let Some(link) = pointer.as_deref() {
            replace_dotgit_with_gitfile(entry, link)?;
        }
        let path = entry.path.as_os_str();
        if locked {
            git::run(
                &repo.root,
                [OsStr::new("worktree"), OsStr::new("unlock"), path],
            )?;
        }
        git::run(
            &repo.root,
            [OsStr::new("worktree"), OsStr::new("remove"), path],
        )?;
        if entry.path.exists() || Entry::load(entry.admin.clone()).is_some() {
            return refuse(
                "git worktree remove returned success but the path or registration remains",
            );
        }
        Ok(())
    })();

    if let Err(fail) = operation {
        let registered = Entry::load(entry.admin.clone()).is_some();
        let still_registered = registered && entry.path.is_dir();
        if !still_registered {
            if let Some(transaction) = &mut transaction {
                transaction.manifest["phase"] = json!("recovery_required");
                let _ = artifact::write_recovery_marker(transaction);
            }
            return Err(Fail::new(
                fail.code,
                format!(
                    "{}; {}; recovery data preserved",
                    fail.msg,
                    if registered {
                        "worktree path changed during removal"
                    } else {
                        "worktree registration disappeared during removal"
                    }
                ),
            ));
        }
        let rollback = rollback_failed_removal(
            repo,
            entry,
            authority,
            &expected_head,
            rollback_dotgit.as_deref(),
            locked,
        );
        if let Err(rollback) = rollback {
            return Err(Fail::new(
                fail.code,
                format!("{}; rollback stopped safely: {}", fail.msg, rollback.msg),
            ));
        }
        if let Some(transaction) = &mut transaction {
            if let Err(restore) =
                artifact::restore(transaction, &entry.path, &entry.admin, &expected_head)
            {
                return Err(Fail::new(
                    fail.code,
                    format!(
                        "{}; artifact rollback stopped safely: {}",
                        fail.msg, restore
                    ),
                ));
            }
        }
        return Err(fail);
    }

    if let Some(transaction) = &mut transaction {
        let report = artifact::finish_transaction(transaction).map_err(|why| {
            Fail::new(
                1,
                format!("worktree removed; quarantine recovery required: {why}"),
            )
        })?;
        for path in report.discarded {
            if json_output {
                let _ = writeln!(
                    std::io::stderr(),
                    "discarded generated artifact {}",
                    path.display()
                );
            } else {
                out!("discarded generated artifact {}", path.display());
            }
        }
        for path in report.preserved {
            if json_output {
                let _ = writeln!(
                    std::io::stderr(),
                    "preserved generated artifact quarantine at {}",
                    path.display()
                );
            } else {
                out!(
                    "preserved generated artifact quarantine at {}",
                    path.display()
                );
            }
        }
        for warning in report.warnings {
            let _ = writeln!(std::io::stderr(), "warning: {warning}");
        }
    }
    Ok(())
}

fn cmd_reap(a: &Args) -> R<()> {
    let repo = a.repo(false)?;
    let (_, main_ref) = base_ref(a, &repo.root)?;
    let grace = match a.get("--grace-secs")? {
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(|| Fail::new(2, "--grace-secs must be a non-negative integer"))?,
        None => DEFAULT_GRACE_SECS,
    };
    let _lock = repo.lock()?;
    let annex_repo = repo.uses_annex()?;
    let all = task::entries(&repo.common);
    let mut candidates = Vec::new();
    for e in &all {
        match &e.meta {
            Meta::Invalid(why) => out!("orphan\t{}\tinvalid marker: {why}", e.id()),
            Meta::Wtctl(m) => match procinfo::liveness(&m.owner) {
                Liveness::Dead if m.claimed_at().is_none_or(|t| now() - t < grace) => {
                    out!("orphan\t{}\twithin the {grace}s grace period", e.id());
                }
                Liveness::Dead => candidates.push(e),
                Liveness::Unknown => {
                    out!("orphan\t{}\towner liveness cannot be verified", e.id())
                }
                Liveness::Alive => {}
            },
            _ => {}
        }
    }
    if !candidates.is_empty() && !a.flag("--dry-run") && !a.flag("--external-writers-quiesced") {
        return refuse(
            "inspect repository-specific writers, then pass --external-writers-quiesced",
        );
    }
    let facts = task::facts(&candidates, &main_ref)?;
    let annex = if annex_repo && !candidates.is_empty() && !a.flag("--dry-run") {
        repo.require_annex_ready(a.flag("--external-writers-quiesced"))?
    } else {
        annex_repo
    };
    let cwds = if candidates.is_empty() {
        procinfo::CwdScan::Unavailable("not scanned".into())
    } else {
        procinfo::scan_cwds()
    };
    for (index, (e, f)) in candidates.iter().zip(&facts).enumerate() {
        let id = e.id();
        // A signal is held back until the lock is released; stop starting new removals.
        if lock::interrupt_pending() {
            for rest in &candidates[index..] {
                out!("orphan\t{}\tkept (interrupted)", rest.id());
            }
            break;
        }
        if !e.path.is_dir() {
            out!("orphan\t{id}\tworktree path is missing; inspect before pruning");
            continue;
        }
        if let Err(why) = f.removable() {
            out!("orphan\t{id}\t{why}; adopt or inspect it");
            continue;
        }
        if a.flag("--dry-run") {
            out!(
                "would-reap\t{id}\t{}\t(process check skipped)",
                e.path.display()
            );
            continue;
        }
        // Re-read state right before removal; anything that moved since is kept.
        let fresh = Entry::load(e.admin.clone());
        let refreshed = match &fresh {
            Some(fe) if fe.head == f.head && fe.marker().is_some() => {
                match task::facts(&[fe], &main_ref) {
                    Ok(v) => v.first().cloned(),
                    Err(_) => {
                        out!("orphan\t{id}\tstatus unavailable");
                        continue;
                    }
                }
            }
            _ => None,
        };
        match (fresh, refreshed) {
            (Some(fe), Some(rf)) if rf.removable().is_ok() => {
                let removal = RemovalAuthority {
                    worktree_id: id.clone(),
                    wtctl_task_id: Some(id.clone()),
                    expected_marker: fe.marker().map(|m| m.raw.clone()),
                    expected_lock_reason: Some(format!("agent-task {id}")),
                    legacy_owner: None,
                    expected_branch: None,
                };
                match remove_worktree(
                    &repo,
                    &fe,
                    &removal,
                    RemovalContext {
                        main_ref: &main_ref,
                        cwds: &cwds,
                        annex,
                        cleanup: artifact::Options {
                            discard_caches: true,
                            ..artifact::Options::default()
                        },
                        json_output: false,
                    },
                ) {
                    Ok(()) => {
                        repo.audit(json!({"at": now(), "event": "reap", "task_id": id}));
                        out!("reaped\t{id}");
                    }
                    Err(fail) => out!("orphan\t{id}\tremoval failed: {}", fail.msg),
                }
            }
            _ => out!("orphan\t{id}\tchanged during reap; kept"),
        }
    }
    Ok(())
}

fn cmd_adopt(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let id = a.need("--id")?;
    let _lock = repo.lock()?;
    let (entry, marker) = repo.find_task(id)?;
    match procinfo::liveness(&marker.owner) {
        Liveness::Dead => {}
        state => {
            return refuse(format!(
                "{id} owner {} pid {} is {}; adopt only tasks whose owner is dead",
                marker.owner.client,
                marker.owner.pid,
                state.as_str()
            ))
        }
    }
    let current = me()?;
    let mut raw = marker.raw.clone();
    let event = json!({"event": "adopt", "at": now(), "from": owner_json(&marker.owner)});
    match raw.get_mut("history").and_then(Value::as_array_mut) {
        Some(h) => h.push(event),
        None => raw["history"] = json!([event]),
    }
    raw["owner"] = owner_json(&current);
    if let Some(session) = show::session_arg(a)? {
        raw["session_hint"] = json!(session);
    }
    task::write_marker(&entry.admin, &raw)?;
    repo.audit(json!({"at": now(), "event": "adopt", "task_id": id, "pid": current.pid}));
    out!("adopted {id}: owner={} pid={}", current.client, current.pid);
    Ok(())
}

fn find_legacy_target(
    repo: &Repo,
    worktree_id: Option<&str>,
    branch: Option<&str>,
) -> R<(Entry, HashMap<String, String>)> {
    let entries = task::entries(&repo.common);
    let matches: Vec<(Entry, HashMap<String, String>)> = entries
        .into_iter()
        .filter(|entry| {
            if let Some(branch) = branch {
                entry
                    .branch_ref()
                    .and_then(|r| r.strip_prefix("refs/heads/"))
                    == Some(branch)
            } else {
                worktree_id.is_some_and(|id| entry.id() == id)
            }
        })
        .filter(|entry| entry.marker().is_none())
        .filter_map(|entry| legacy_metadata(repo, &entry).map(|meta| (entry, meta)))
        .collect();
    match matches.len() {
        0 => refuse("no matching legacy worktree with readable owner metadata"),
        1 => Ok(matches.into_iter().next().expect("one match")),
        n => refuse(format!("legacy selector matched {n} worktrees")),
    }
}

fn delete_legacy_branch(repo: &Repo, branch: &str, expected_head: &str, base_ref: &str) -> R<()> {
    if branch.starts_with('-') {
        return Err(Fail::new(2, "branch must not start with '-'"));
    }
    let name = git::output(&repo.root, ["check-ref-format", "--branch", branch])?;
    if !name.status.success() {
        return Err(Fail::new(2, format!("invalid branch name {branch:?}")));
    }
    let refname = format!("refs/heads/{branch}");
    if main_checkout(repo, &refname)?.is_some() {
        return refuse(format!("{refname} is still checked out in a worktree"));
    }
    let current = git::run(&repo.root, ["rev-parse", "--verify", &refname])?;
    if current.trim() != expected_head {
        return refuse(format!("{refname} moved after verification; branch kept"));
    }
    let base = git::run(&repo.root, ["rev-parse", "--verify", base_ref])?;
    if !is_ancestor(&repo.root, expected_head, base.trim())? {
        return refuse(format!(
            "{refname} is not contained in {base_ref}; branch kept"
        ));
    }
    let hooks = active_hooks(&repo.root, &["reference-transaction"], None, None)?;
    check_hooks(&hooks, false)?;
    let removed = git::output(&repo.root, ["update-ref", "-d", &refname, expected_head])?;
    if !removed.status.success() {
        return refuse(format!(
            "could not delete {refname}: {}",
            git::stderr_of(&removed)
        ));
    }
    let section = format!("branch.{branch}");
    let config = git::output(
        &repo.root,
        [
            "config",
            "--file",
            &repo.common.join("config").to_string_lossy(),
            "--remove-section",
            &section,
        ],
    )?;
    if !config.status.success() && !git::stderr_of(&config).contains("no such section") {
        return Err(Fail::new(
            1,
            format!(
                "{refname} was deleted but its config section remains: {}",
                git::stderr_of(&config)
            ),
        ));
    }
    Ok(())
}

fn cmd_remove(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    if a.get("--lock-name")?
        .is_some_and(|name| name != lock::LEGACY_LOCK)
    {
        return Err(Fail::new(
            2,
            format!(
                "the shared lifecycle lock name is fixed at {}",
                lock::LEGACY_LOCK
            ),
        ));
    }
    if let Some(operation_id) = a.get("--purge-preserved-quarantine")? {
        if !a.flag("--dry-run") && !a.flag("--external-writers-quiesced") {
            return refuse(
                "inspect quarantine and repository writers, then pass --external-writers-quiesced",
            );
        }
        if ["--id", "--legacy-worktree-id", "--expect-branch"]
            .iter()
            .any(|selector| a.os(selector).is_some())
            || [
                "--delete-branch",
                "--discard-direnv-cache",
                "--discard-generated-caches",
                "--discard-generated-build-outputs",
                "--discard-generated-artifacts",
                "--preserve-generated-artifacts",
                "--allow-unreferenced-reflog",
            ]
            .iter()
            .any(|flag| a.flag(flag))
            || a.os("--base").is_some()
        {
            return Err(Fail::new(
                2,
                "quarantine purge cannot be combined with worktree removal options",
            ));
        }
        let (_, main_ref) = main_ref(a, &repo.root)?;
        let _lock = repo.lock()?;
        let labels = artifact::purge_preserved(
            &repo.root,
            &repo.name(),
            &repo.common,
            &main_ref,
            operation_id,
            a.flag("--discard-unverified-direnv"),
            a.flag("--dry-run"),
        )
        .map_err(|why| Fail::new(4, why))?;
        if a.flag("--dry-run") {
            if a.flag("--json") {
                out!("{}", json!({"status": "dry-run", "artifacts": labels}));
            } else {
                out!(
                    "dry-run ok: would purge preserved artifacts: {}",
                    labels.join(", ")
                );
            }
        } else {
            if a.flag("--json") {
                out!("{}", json!({"status": "purged", "artifacts": labels}));
            } else {
                out!("purged preserved artifacts: {}", labels.join(", "));
            }
        }
        return Ok(());
    }
    if a.flag("--discard-unverified-direnv") {
        return Err(Fail::new(
            2,
            "--discard-unverified-direnv requires --purge-preserved-quarantine",
        ));
    }
    let cleanup = artifact_options(a, false)?;
    let id = a.get("--id")?;
    let legacy_id = a.get("--legacy-worktree-id")?;
    let branch = a.get("--expect-branch")?;
    if [id, legacy_id, branch]
        .into_iter()
        .filter(Option::is_some)
        .count()
        != 1
    {
        return Err(Fail::new(
            2,
            "provide exactly one of --id, --legacy-worktree-id, or --expect-branch",
        ));
    }
    if a.flag("--delete-branch") && branch.is_none() {
        return Err(Fail::new(2, "--delete-branch requires --expect-branch"));
    }
    let abandon = a.flag("--abandon");
    if abandon
        && (id.is_none() || a.os("--base").is_some() || a.flag("--allow-unreferenced-reflog"))
    {
        return Err(Fail::new(
            2,
            "--abandon needs --id and cannot be combined with --base or --allow-unreferenced-reflog",
        ));
    }
    let (_, mut main_ref) = base_ref(a, &repo.root)?;
    let external_writers_quiesced = a.flag("--external-writers-quiesced") || a.flag("--dry-run");
    let annex = repo.require_annex_ready(external_writers_quiesced)?;
    if !a.flag("--dry-run") && !external_writers_quiesced {
        return refuse(
            "inspect repository-specific writers, then pass --external-writers-quiesced",
        );
    }
    let _lock = repo.lock()?;
    let (entry, authority) = if let Some(id) = id {
        let (entry, marker) = repo.find_task(id)?;
        require_owner(&marker)?;
        let authority = RemovalAuthority {
            worktree_id: entry.id(),
            wtctl_task_id: Some(marker.task_id.clone()),
            expected_marker: Some(marker.raw.clone()),
            expected_lock_reason: Some(format!("agent-task {id}")),
            legacy_owner: None,
            expected_branch: None,
        };
        (entry, authority)
    } else {
        let (entry, trailer) = find_legacy_target(&repo, legacy_id, branch)?;
        if !legacy_is_mine(&trailer) {
            return refuse("legacy worktree belongs to another or unknown client session");
        }
        if a.flag("--delete-branch") && branch.is_none() {
            return Err(Fail::new(
                2,
                "--delete-branch is only valid for a branch-backed legacy worktree",
            ));
        }
        let authority = RemovalAuthority {
            worktree_id: entry.id(),
            wtctl_task_id: None,
            expected_marker: None,
            expected_lock_reason: None,
            legacy_owner: Some((trailer["client"].clone(), trailer["session"].clone())),
            expected_branch: branch.map(str::to_string),
        };
        (entry, authority)
    };
    let mut abandoned_ref = None;
    if abandon {
        // Commits reachable from the preserved tip count as kept, so every
        // other removal check still applies (reflog-only commits still refuse).
        let tip = entry
            .current_oid()
            .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD"))?;
        let refname = if a.flag("--dry-run") {
            abandon::ref_name(&authority.worktree_id)
        } else {
            abandon::preserve(&repo, &authority.worktree_id, &tip)?
        };
        main_ref = tip;
        abandoned_ref = Some(refname);
    }
    let facts = task::facts(&[&entry], &main_ref)?;
    let f = facts.first().cloned().unwrap_or_default();
    f.removable_except_status_with_reflog_opt_in(cleanup.allow_unreferenced_reflog)
        .or_else(|why| refuse(format!("{}: {why}", authority.worktree_id)))?;
    if cleanup.allow_unreferenced_reflog {
        let main_tip = git::run(&repo.root, ["rev-parse", "--verify", &main_ref])?;
        if !is_ancestor(&repo.root, &f.head, main_tip.trim())? {
            return refuse("--allow-unreferenced-reflog cannot remove an unintegrated HEAD");
        }
    }
    let head = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD"))?;
    if a.flag("--dry-run") {
        let main_worktree = canonical_main_checkout(&repo)?;
        artifact::check_pending_transaction(&repo.name(), &entry.path)
            .map_err(|why| Fail::new(4, why))?;
        let plan = artifact::inspect_plan(&entry.path, main_worktree.as_deref(), cleanup)
            .map_err(|why| Fail::new(4, why))?;
        verify_removal_authority(&repo, &entry, &authority)?;
        removal_lock_state(&entry, &authority)?;
        let users = procinfo::users_inside(&procinfo::scan_cwds(), &entry.path)
            .map_err(|e| Fail::new(4, format!("cannot check processes inside worktree: {e}")))?;
        if !users.is_empty() {
            return refuse(format!(
                "processes still have their cwd inside {} (pids: {})",
                entry.path.display(),
                users.join(", ")
            ));
        }
        let dotgit = gitfile_pointer(&entry)?;
        if a.flag("--json") {
            out!(
                "{}",
                json!({
                    "status": "dry-run",
                    "worktree_id": authority.worktree_id,
                    "path": entry.path,
                    "head": head,
                    "verified_dotgit": if dotgit.is_some() { "symlink" } else { "gitfile" },
                    "generated_artifacts": plan.generated.len() + usize::from(plan.direnv.is_some()),
                    "canonical_database_link": plan.database_link.is_some(),
                    "git_annex_restage": annex,
                })
            );
        } else {
            out!(
                "dry-run ok: {} (.git is a {}; HEAD {}) would be removed{}{}",
                entry.path.display(),
                if dotgit.is_some() {
                    "verified symlink"
                } else {
                    "verified gitfile"
                },
                head,
                if annex {
                    "; git annex restage would run first"
                } else {
                    ""
                },
                if plan.generated.is_empty()
                    && plan.direnv.is_none()
                    && plan.database_link.is_none()
                {
                    String::new()
                } else {
                    format!(
                        "; {} generated artifact(s) would be quarantined then cleaned or preserved",
                        plan.generated.len() + usize::from(plan.direnv.is_some())
                    )
                },
            );
        }
        return Ok(());
    }
    remove_worktree(
        &repo,
        &entry,
        &authority,
        RemovalContext {
            main_ref: &main_ref,
            cwds: &procinfo::scan_cwds(),
            annex,
            cleanup,
            json_output: a.flag("--json"),
        },
    )?;
    if let Some(branch) = branch {
        if a.flag("--delete-branch") {
            delete_legacy_branch(&repo, branch, &head, &main_ref)?;
        }
    }
    repo.audit(json!({"at": now(), "event": "remove", "worktree_id": authority.worktree_id, "abandoned_ref": abandoned_ref}));
    if a.flag("--json") {
        out!(
            "{}",
            json!({"status": "removed", "worktree_id": authority.worktree_id, "branch_deleted": a.flag("--delete-branch"), "abandoned_ref": abandoned_ref})
        );
    } else {
        out!("removed {}", authority.worktree_id);
        if let Some(refname) = &abandoned_ref {
            out!(
                "preserved {head} as {refname}; restore with `wtctl restore --id {}`",
                authority.worktree_id
            );
        }
    }
    Ok(())
}

fn run(argv: &[OsString]) -> R<()> {
    let Some((cmd, rest)) = argv.split_first() else {
        return Err(Fail::new(2, USAGE));
    };
    let cmd = cmd.to_str().unwrap_or("");
    if matches!(cmd, "-V" | "--version" | "version") {
        out!("wtctl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if matches!(cmd, "-h" | "--help") || (cmd == "help" && rest.is_empty()) {
        out!("{USAGE}");
        return Ok(());
    }
    let help_target = if cmd == "help" {
        rest.first().and_then(|name| name.to_str())
    } else if rest
        .iter()
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
    {
        Some(cmd)
    } else {
        None
    };
    if let Some(name) = help_target {
        return command_help(name);
    }
    let args = Args::parse(rest)?;
    let cleanup_flags = [
        "--discard-direnv-cache",
        "--discard-generated-caches",
        "--discard-generated-build-outputs",
        "--discard-generated-artifacts",
        "--preserve-generated-artifacts",
        "--discard-unverified-direnv",
        "--allow-unreferenced-reflog",
    ];
    if !matches!(cmd, "finish" | "remove")
        && (cleanup_flags.iter().any(|flag| args.flag(flag))
            || args.os("--purge-preserved-quarantine").is_some())
    {
        return Err(Fail::new(
            2,
            "artifact cleanup options are only valid for finish or remove",
        ));
    }
    if cmd == "finish"
        && (args.flag("--discard-unverified-direnv")
            || args.os("--purge-preserved-quarantine").is_some())
    {
        return Err(Fail::new(
            2,
            "quarantine purge options are only valid for remove",
        ));
    }
    if !matches!(cmd, "finish" | "integrate") && args.flag("--allow-annex-filter") {
        return Err(Fail::new(
            2,
            "--allow-annex-filter is only valid for integrate or finish",
        ));
    }
    if cmd != "finish"
        && (args.flag("--allow-commit-signing")
            || args.flag("--recover-failed-merge")
            || args.flag("--keep-worktree")
            || args.os("--integration-message").is_some())
    {
        return Err(Fail::new(
            2,
            "merge integration options are only valid for finish",
        ));
    }
    if args.os("--base").is_some() && !matches!(cmd, "list" | "reap" | "remove") {
        return Err(Fail::new(
            2,
            "--base is only valid for list, reap, or remove",
        ));
    }
    if cmd != "create" && (args.os("--parent").is_some() || args.os("--worktrees-root").is_some()) {
        return Err(Fail::new(
            2,
            "--parent and --worktrees-root are only valid for create",
        ));
    }
    if !matches!(cmd, "create" | "adopt" | "describe") && args.os("--session").is_some() {
        return Err(Fail::new(
            2,
            "--session is only valid for create, adopt, or describe",
        ));
    }
    if cmd != "finish" && (args.flag("--squash") || args.flag("--rebase")) {
        return Err(Fail::new(
            2,
            "--squash and --rebase are only valid for finish",
        ));
    }
    if cmd != "prune"
        && (args.flag("--quarantine")
            || args.flag("--refs")
            || args.os("--older-than-days").is_some())
    {
        return Err(Fail::new(
            2,
            "--quarantine, --refs, and --older-than-days are only valid for prune",
        ));
    }
    if cmd != "doctor" && args.flag("--fix-stale-lock") {
        return Err(Fail::new(2, "--fix-stale-lock is only valid for doctor"));
    }
    if cmd != "remove" && args.flag("--abandon") {
        return Err(Fail::new(2, "--abandon is only valid for remove"));
    }
    if cmd != "list" && args.flag("--abandoned") {
        return Err(Fail::new(2, "--abandoned is only valid for list"));
    }
    if cmd != "remove" && args.os("--lock-name").is_some() {
        return Err(Fail::new(2, "--lock-name is only valid for remove"));
    }
    match cmd {
        "capabilities" => cmd_capabilities(&args),
        "create" => cmd_create(&args),
        "whoami" => cmd_whoami(&args),
        "list" => cmd_list(&args),
        "integrate" => cmd_integrate(&args),
        "finish" => cmd_finish(&args),
        "reap" => cmd_reap(&args),
        "adopt" => cmd_adopt(&args),
        "restore" => abandon::cmd_restore(&args),
        "doctor" => doctor::cmd_doctor(&args),
        "quarantine" => housekeeping::cmd_quarantine(&args),
        "prune" => housekeeping::cmd_prune(&args),
        "show" => show::cmd_show(&args),
        "describe" => show::cmd_describe(&args),
        "remove" => cmd_remove(&args),
        _ => Err(Fail::new(2, format!("unknown command {cmd:?}\n{USAGE}"))),
    }
}

fn main() {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let json_output = argv.iter().any(|arg| arg.as_bytes() == b"--json");
    let result = run(&argv);
    // An empty message means the command already printed its own report.
    if let Some(fail) = result.as_ref().err().filter(|fail| !fail.msg.is_empty()) {
        if json_output {
            out!(
                "{}",
                json!({
                    "status": "error",
                    "error": fail.msg,
                    "exit_code": fail.code,
                })
            );
        } else {
            let _ = writeln!(std::io::stderr(), "error: {}", fail.msg);
        }
    }
    // Interrupts held back during a locked mutation take effect only after the
    // error above is written, so a caller never loses it.
    lock::restore_signals();
    if let Err(fail) = result {
        std::process::exit(fail.code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = git::command(dir, args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wtctl-unit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let _ = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&dir)
            .status();
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "a"]);
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "b"]);
        dir
    }

    #[test]
    fn cas_update_refuses_a_stale_old_value() {
        let dir = scratch("cas");
        let b = sh(&dir, &["rev-parse", "HEAD"]);
        let a = sh(&dir, &["rev-parse", "HEAD~1"]);
        sh(&dir, &["update-ref", "refs/heads/side", &a]);
        // Another writer moved the ref after we recorded `a`.
        sh(&dir, &["update-ref", "refs/heads/side", &b]);
        let err = cas_update(&dir, "refs/heads/side", &a, &a, "t").unwrap_err();
        assert_eq!(err.code, 4);
        assert!(ensure_ref(&dir, "refs/heads/side", &a).is_err());
        assert!(cas_update(&dir, "refs/heads/side", &a, &b, "t").is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_status_index_flags_and_oids() {
        let c = git::parse_status(b"?? new\0!! __pycache__/\0!! keep.db\0R  a\0b\0 M c\0");
        assert_eq!((c.tracked, c.untracked, c.ignored), (2, 1, 1));
        assert_eq!(git::count_hidden(b"H a\0h b\0S c\0s d\0"), 3);
        assert!(git::is_oid(&"a".repeat(40)) && git::is_oid(&"0".repeat(64)));
        assert!(!git::is_oid("ref: refs/heads/main") && !git::is_oid(&"A".repeat(40)));
    }

    #[test]
    fn annex_dotgit_symlink_round_trips_to_a_verified_gitfile() {
        let dir = std::env::temp_dir().join(format!("wtctl-annex-dotgit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let worktree = dir.join("worktree");
        let admin = dir.join("admin");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&admin).unwrap();
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        std::os::unix::fs::symlink(&admin, worktree.join(".git")).unwrap();
        let entry = Entry {
            admin: admin.clone(),
            path: worktree.clone(),
            head: "test-head".into(),
            lock_reason: None,
            meta: Meta::None,
        };
        let original = fs::read_link(worktree.join(".git"))
            .unwrap()
            .into_os_string();
        assert_eq!(gitfile_pointer(&entry).unwrap(), Some(original.clone()));

        replace_dotgit_with_gitfile(&entry, &original).unwrap();
        assert!(gitfile_pointer(&entry).unwrap().is_none());
        assert!(fs::read_to_string(worktree.join(".git"))
            .unwrap()
            .starts_with("gitdir: "));

        restore_dotgit_symlink(&entry, &original).unwrap();
        assert_eq!(gitfile_pointer(&entry).unwrap(), Some(original));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tracked_content_verification_rejects_a_symlink_parent() {
        let dir = std::env::temp_dir().join(format!("wtctl-parent-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let worktree = dir.join("worktree");
        let outside = dir.join("outside");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("file.txt"), b"tracked content\n").unwrap();
        std::os::unix::fs::symlink(&outside, worktree.join("linked")).unwrap();
        assert!(!tracked_parent_dirs_are_real(&worktree, Path::new("linked/file.txt")).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn annex_hash_fallback_never_runs_a_custom_filter() {
        for source in ["worktree", "info", "core", "annex-no-ack"] {
            let dir = scratch(&format!("custom-filter-{source}"));
            let annex_attributes = source != "core";
            if annex_attributes {
                fs::write(dir.join(".gitattributes"), "-foo.bin filter=annex\n").unwrap();
                sh(&dir, &["config", "filter.annex.clean", "cat"]);
                sh(&dir, &["add", ".gitattributes"]);
                sh(&dir, &["commit", "-q", "-m", "annex attributes"]);
            }
            fs::write(dir.join("-foo.bin"), b"pointer\n").unwrap();
            sh(&dir, &["add", "--", "-foo.bin"]);
            sh(&dir, &["commit", "-q", "-m", "pointer blob"]);
            let expected_tree = sh(&dir, &["rev-parse", "HEAD^{tree}"]);
            fs::write(dir.join("-foo.bin"), b"materialized content\n").unwrap();

            let marker = dir.join("custom-filter-ran");
            let command = format!("sh -c 'touch {}; cat'", marker.display());
            if source == "annex-no-ack" {
                sh(&dir, &["config", "filter.annex.clean", &command]);
            } else {
                sh(&dir, &["config", "filter.custom.clean", &command]);
            }
            match source {
                "worktree" => {
                    fs::write(dir.join(".gitattributes"), "-foo.bin filter=custom\n").unwrap()
                }
                "info" => {
                    fs::write(dir.join(".git/info/attributes"), "-foo.bin filter=custom\n").unwrap()
                }
                "core" => {
                    let attributes = dir.join("external.attributes");
                    fs::write(&attributes, "-foo.bin filter=custom\n").unwrap();
                    sh(
                        &dir,
                        &[
                            "config",
                            "core.attributesFile",
                            attributes.to_str().unwrap(),
                        ],
                    );
                }
                "annex-no-ack" => {}
                _ => unreachable!(),
            }

            let repo = Repo::open(&dir).unwrap();
            assert!(
                !tracked_worktree_matches_index(
                    &repo,
                    &dir,
                    &expected_tree,
                    source != "annex-no-ack",
                )
                .unwrap(),
                "{source} override must prevent filter-aware matching"
            );
            assert!(
                !marker.exists(),
                "custom clean filter ran for {source} attributes"
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn filter_attribute_batch_drains_output_while_writing_long_paths() {
        let dir = scratch("filter-attribute-pipes");
        let first = "a".repeat(180);
        let second = "b".repeat(180);
        let paths: Vec<Vec<u8>> = (0..FILTER_SCAN_BATCH_PATHS)
            .map(|index| format!("{first}/{index}{second}/{first}").into_bytes())
            .collect();
        let path_count = paths.len();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_dir = dir.clone();
        std::thread::spawn(move || {
            let _ = sender.send(worktree_filter_attributes(&worker_dir, None, &paths));
        });

        let attributes = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("filter attribute batching deadlocked")
            .unwrap();
        assert_eq!(attributes.len(), path_count);
        assert!(attributes.iter().all(|value| value == b"unspecified"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn abort_restore_path_conflicts_use_component_boundaries() {
        assert!(paths_overlap(b"restore", b"restore/child"));
        assert!(paths_overlap(b"restore/child", b"restore/"));
        assert!(paths_overlap(b"restore", b"restore"));
        assert!(!paths_overlap(b"restore/a", b"restore/b"));
        assert!(!paths_overlap(b"restore", b"restore-old"));
    }

    #[test]
    fn open_path_collection_recurses_without_following_symlinks() {
        let dir = std::env::temp_dir().join(format!("wtctl-open-paths-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("ignored");
        let external = dir.join("external");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::write(root.join("nested/file"), b"ignored data").unwrap();
        fs::write(external.join("outside"), b"external data").unwrap();
        std::os::unix::fs::symlink(&external, root.join("external-link")).unwrap();
        let mut paths = BTreeSet::new();
        let mut count = 0;

        collect_open_paths(&root, &mut paths, &mut count).unwrap();

        assert!(paths.contains(&root.join("nested/file")));
        assert!(!paths.contains(&root.join("external-link")));
        assert!(!paths.contains(&external.join("outside")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_legacy_trailer() {
        let t = task::legacy_trailer("fix thing client=claude session=unknown base=abc pid=42@34");
        assert_eq!(t["client"], "claude");
        assert_eq!(t["session"], "unknown");
        assert_eq!(t["pid"], "42@34");
        assert_eq!(t["task"], "fix thing");
        assert_eq!(
            task::legacy_liveness(&task::legacy_trailer("x client=c")),
            Liveness::Unknown
        );
        // Liveness is judged from injected observations, not from a guessed free pid.
        use procinfo::Existence::{Forbidden, Missing, Present};
        assert_eq!(task::judge_legacy(Missing, false, None, 34), Liveness::Dead);
        assert_eq!(
            task::judge_legacy(Present, true, Some(34), 34),
            Liveness::Dead
        );
        assert_eq!(
            task::judge_legacy(Present, false, Some(37), 34),
            Liveness::Alive
        );
        assert_eq!(
            task::judge_legacy(Present, false, Some(90), 34),
            Liveness::Dead
        );
        assert_eq!(
            task::judge_legacy(Present, false, None, 34),
            Liveness::Unknown
        );
        assert_eq!(
            task::judge_legacy(Forbidden, false, None, 34),
            Liveness::Unknown
        );
    }

    #[test]
    fn validates_ids_and_marker_schema() {
        assert!(task::valid_task_id("fix-thing-20260101-0a1b2c3d"));
        assert!(!task::valid_task_id("fix-20260101-0a1b2c3d"));
        assert!(!task::valid_task_id("fix-thing-2026011-0a1b2c3d"));
        assert!(!task::valid_task_id("fix-thing-20260101-0A1B2C3D"));
        let mut m = json!({"schema": 1, "tool": "wtctl", "task_id": "fix-thing-20260101-0a1b2c3d",
            "worktree": "/w", "base": "a".repeat(40),
            "owner": {"pid": 42, "start": "1.0", "client": "claude"}});
        assert!(Marker::parse(m.clone()).is_some());
        m["schema"] = json!(2);
        assert!(Marker::parse(m.clone()).is_none());
        m["schema"] = json!(1);
        m["owner"]["pid"] = json!(1);
        assert!(Marker::parse(m).is_none());
    }

    fn linked(dir: &Path, name: &str) -> (PathBuf, Entry) {
        let wt = dir.join(name);
        sh(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &wt.to_string_lossy(),
                "HEAD",
            ],
        );
        let admin = dir.join(".git/worktrees").join(name);
        (wt, Entry::load(admin).unwrap())
    }

    fn reload(e: &Entry) -> Entry {
        Entry::load(e.admin.clone()).unwrap()
    }

    #[test]
    fn facts_count_reflog_only_commits_against_the_main_ref() {
        let dir = scratch("revlist");
        let (wt, e) = linked(&dir, "wt");
        sh(&wt, &["commit", "-q", "--allow-empty", "-m", "c"]);
        let c = sh(&wt, &["rev-parse", "HEAD"]);
        sh(&wt, &["reset", "-q", "--hard", "HEAD~1"]);
        let f = task::facts(&[&reload(&e)], "refs/heads/main").unwrap();
        assert_eq!(f[0].unintegrated, Some(1));
        assert!(f[0].removable().is_err());
        // `^refs/heads/main` excludes everything main already contains.
        sh(&dir, &["update-ref", "refs/heads/main", &c]);
        let f = task::facts(&[&reload(&e)], "refs/heads/main").unwrap();
        assert_eq!(f[0].unintegrated, Some(0));
        assert!(f[0].removable().is_ok(), "{:?}", f[0].removable());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn facts_count_commits_held_only_by_per_worktree_refs() {
        let dir = scratch("wtref");
        let (wt, e) = linked(&dir, "wt");
        let tree = sh(&wt, &["rev-parse", "HEAD^{tree}"]);
        let lone = sh(&wt, &["commit-tree", &tree, "-p", "HEAD", "-m", "lone"]);
        sh(&wt, &["update-ref", "refs/worktree/keep", &lone]);
        let f = task::facts(&[&reload(&e)], "refs/heads/main").unwrap();
        // The files backend also trips the admin refs/ blocker; the count is independent.
        assert_eq!(f[0].unintegrated, Some(1));
        sh(&dir, &["update-ref", "refs/heads/main", &lone]);
        let f = task::facts(&[&reload(&e)], "refs/heads/main").unwrap();
        assert_eq!(f[0].unintegrated, Some(0));
        fs::remove_dir_all(&dir).unwrap();
    }
}
