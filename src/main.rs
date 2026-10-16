//! wtctl: create, inspect, integrate, and clean up detached task worktrees.
//!
//! Ownership is the creating agent process (pid + start time) found among the
//! caller's ancestors, never a session-id environment variable. Mutating
//! commands serialize on an flock and the legacy mkdir lock.

mod git;
mod lock;
mod procinfo;
mod task;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use procinfo::{Liveness, Owner};
use serde_json::{json, Value};
use task::{canon, owner_json, Entry, Marker, Meta};

const DEFAULT_GRACE_SECS: i64 = 600;

const USAGE: &str = "usage: wtctl <command> [options]

  create  --repo <path> --slug <slug> [--task <text>] [--from <commit>] [--allow-repository-hooks] [--json]
  whoami  [--path <path>] [--json]
  list    [--repo <path>] [--mine | --orphans | --all] [--main <branch>] [--json]
  integrate --repo <path> --id <task-id> [--main <branch>] [--allow-repository-hooks]
  reap    [--repo <path>] [--dry-run] [--grace-secs <n>] [--main <branch>]
  adopt   --repo <path> --id <task-id>
  remove  --repo <path> --id <task-id> [--main <branch>]

exit codes: 0 ok, 1 error, 2 usage, 3 main diverged (rebase needed),
4 refused by a safety check, 5 lock busy";

macro_rules! out {
    ($($arg:tt)*) => {{
        // A closed stdout (e.g. `| head`) must not abort a mutation midway.
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

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
];
const FLAG_OPTS: &[&str] = &[
    "--mine",
    "--orphans",
    "--all",
    "--dry-run",
    "--json",
    "--allow-repository-hooks",
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
            "no coding-agent ancestor process (claude/opencode) found; \
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
    let hooks = active_hooks(
        &repo.root,
        &["post-checkout", "reference-transaction"],
        Some(&base),
        Some(Path::new(OsStr::from_bytes(hooks_dir))),
    )?;
    check_hooks(&hooks, a.flag("--allow-repository-hooks"))?;

    let root_dir = state_home()?.join(repo.name()).join("worktrees");
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
    let session_hint = std::env::var(session_var).ok().filter(|v| !v.is_empty());
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
                   "owner_state": state.as_str(), "mine": mine})
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
    }
    Ok(())
}

fn owner_state(meta: &Meta) -> Liveness {
    match meta {
        Meta::Wtctl(m) => procinfo::liveness(&m.owner),
        Meta::Legacy(t) => task::legacy_liveness(t),
        _ => Liveness::Unknown,
    }
}

fn cmd_list(a: &Args) -> R<()> {
    let repo = a.repo(false)?;
    let (_, main_ref) = main_ref(a, &repo.root)?;
    let modes: Vec<&str> = ["--mine", "--orphans", "--all"]
        .into_iter()
        .filter(|f| a.flag(f))
        .collect();
    if modes.len() > 1 {
        return Err(Fail::new(2, "choose one of --mine, --orphans, --all"));
    }
    let mode = modes.first().copied().unwrap_or("--wtctl");
    let current = if mode == "--mine" {
        Some(me()?)
    } else {
        procinfo::find_agent()
    };
    let all = task::entries(&repo.common);
    let selected: Vec<&Entry> = all
        .iter()
        .filter(|e| match (e.marker(), mode) {
            (_, "--all") => true,
            (Some(m), "--mine") => is_mine(m, current.as_ref()),
            (Some(m), "--orphans") => procinfo::liveness(&m.owner) == Liveness::Dead,
            (Some(_), "--wtctl") => true,
            _ => false,
        })
        .collect();
    let facts = task::facts(&selected, &main_ref)?;
    let mut rows = Vec::new();
    for (e, f) in selected.iter().zip(&facts) {
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
                false,
            ),
            Meta::Invalid(_) => ("invalid", String::new(), false),
            Meta::None => ("none", String::new(), false),
        };
        rows.push(json!({
            "id": e.id(), "kind": kind, "path": e.path.to_string_lossy(), "head": e.head,
            "locked": e.lock_reason.is_some(), "lock_reason": e.lock_reason, "owner": owner, "owner_state": owner_state(&e.meta).as_str(),
            "mine": mine, "exists": e.path.is_dir(),
            "clean": f.clean.map(|c| c.is_clean()),
            "changes": f.clean.map(|c| json!({"tracked": c.tracked, "untracked": c.untracked, "ignored": c.ignored})),
            "hidden": f.hidden, "in_progress": f.in_progress, "blocker": f.blocker,
            "unintegrated": f.unintegrated, "integrated": f.unintegrated.map(|n| n == 0),
            "removable": f.removable().err().map_or(Value::Bool(true), Value::String),
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
        out!(
            "{}\t{}\towner={} {}{}\tintegrated={} unintegrated={}\tclean={}\tlocked={}\t{}",
            r["id"].as_str().unwrap_or(""),
            r["kind"].as_str().unwrap_or(""),
            r["owner"].as_str().unwrap_or(""),
            r["owner_state"].as_str().unwrap_or(""),
            if r["mine"].as_bool() == Some(true) {
                " (mine)"
            } else {
                ""
            },
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

fn cmd_integrate(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let id = a.need("--id")?;
    let (main, refname) = main_ref(a, &repo.root)?;
    let _lock = repo.lock()?;
    let (entry, marker) = repo.find_task(id)?;
    let caller = require_owner(&marker)?;
    let Some(tip) = entry.detached_oid().map(str::to_string) else {
        return refuse(format!("{id} is not on a detached HEAD"));
    };
    let facts = task::facts(&[&entry], &refname)?;
    let f = facts.first().cloned().unwrap_or_default();
    if let Some(op) = f.in_progress {
        return refuse(format!("{id} has a {op} in progress"));
    }
    match f.clean {
        Some(c) if c.tracked + c.untracked == 0 => {}
        Some(c) => {
            return refuse(format!(
                "{id} has uncommitted changes ({} tracked, {} untracked); commit them first",
                c.tracked, c.untracked
            ))
        }
        None => return refuse(format!("cannot read the status of {id}")),
    }
    if f.hidden != Some(0) {
        return refuse(format!("{id} has assume-unchanged/skip-worktree entries"));
    }
    let old = git::run(&repo.root, ["rev-parse", "--verify", "--quiet", &refname])
        .map_err(|_| Fail::new(1, format!("{refname} does not exist")))?
        .trim()
        .to_string();

    if is_ancestor(&repo.root, &tip, &old)? {
        out!("{id} is already integrated into {main} ({old})");
        return Ok(());
    }
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
        for path in &conflicts {
            out!("conflict\t{path}");
        }
        return Err(Fail::new(
            3,
            format!("{main} advanced past the task base; merge {main} into the {id} worktree, commit, and retry; do not rebase ({detail})"),
        ));
    }

    let msg = format!("wtctl integrate {id}");
    let allow = a.flag("--allow-repository-hooks");
    match main_checkout(&repo, &refname)? {
        Some(checkout) => {
            check_hooks(
                &active_hooks(
                    &checkout,
                    &["post-merge", "reference-transaction"],
                    None,
                    None,
                )?,
                allow,
            )?;
            let st = git::output(
                &checkout,
                ["status", "--porcelain", "-z", "--untracked-files=normal"],
            )?;
            if !st.status.success() || !st.stdout.is_empty() {
                return refuse(format!("{} is not clean", checkout.display()));
            }
            ensure_ref(&repo.root, &refname, &old)?;
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
                allow,
            )?;
            cas_update(&repo.root, &refname, &tip, &old, &msg)?;
        }
    }
    ensure_ref(&repo.root, &refname, &tip)?;
    repo.audit(
        json!({"at": now(), "event": "integrate", "task_id": id, "ref": refname,
                      "old": old, "new": tip, "pid": caller.pid}),
    );
    out!("integrated {id}: {main} {old}..{tip}");
    Ok(())
}

fn remove_worktree(repo: &Repo, entry: &Entry, id: &str, cwds: &procinfo::CwdScan) -> R<()> {
    if let Ok(cwd) = std::env::current_dir() {
        if canon(&cwd).starts_with(&entry.path) {
            return refuse(format!("run this outside {}", entry.path.display()));
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
    let ours = format!("agent-task {id}");
    let locked = match &entry.lock_reason {
        Some(reason) if *reason == ours => true,
        Some(reason) => {
            return refuse(format!(
                "{} is locked by someone else ({reason:?}); leave it alone",
                entry.path.display()
            ))
        }
        None => false,
    };
    let path = entry.path.as_os_str();
    if locked {
        git::run(
            &repo.root,
            [OsStr::new("worktree"), OsStr::new("unlock"), path],
        )?;
    }
    if let Err(fail) = git::run(
        &repo.root,
        [OsStr::new("worktree"), OsStr::new("remove"), path],
    ) {
        if locked {
            if let Err(e) = relock(repo, path, id) {
                return Err(Fail::new(
                    fail.code,
                    format!("{}; worktree left UNLOCKED ({})", fail.msg, e.msg),
                ));
            }
        }
        return Err(fail);
    }
    Ok(())
}

fn cmd_reap(a: &Args) -> R<()> {
    let repo = a.repo(false)?;
    let (_, main_ref) = main_ref(a, &repo.root)?;
    let grace = match a.get("--grace-secs")? {
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(|| Fail::new(2, "--grace-secs must be a non-negative integer"))?,
        None => DEFAULT_GRACE_SECS,
    };
    let _lock = repo.lock()?;
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
    let facts = task::facts(&candidates, &main_ref)?;
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
                match remove_worktree(&repo, &fe, &id, &cwds) {
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
    task::write_marker(&entry.admin, &raw)?;
    repo.audit(json!({"at": now(), "event": "adopt", "task_id": id, "pid": current.pid}));
    out!("adopted {id}: owner={} pid={}", current.client, current.pid);
    Ok(())
}

fn cmd_remove(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let id = a.need("--id")?;
    let (_, main_ref) = main_ref(a, &repo.root)?;
    let _lock = repo.lock()?;
    let (entry, marker) = repo.find_task(id)?;
    require_owner(&marker)?;
    let facts = task::facts(&[&entry], &main_ref)?;
    let f = facts.first().cloned().unwrap_or_default();
    f.removable()
        .or_else(|why| refuse(format!("{id}: {why}")))?;
    remove_worktree(&repo, &entry, id, &procinfo::scan_cwds())?;
    repo.audit(json!({"at": now(), "event": "remove", "task_id": id}));
    out!("removed {id}");
    Ok(())
}

fn run(argv: &[OsString]) -> R<()> {
    let Some((cmd, rest)) = argv.split_first() else {
        return Err(Fail::new(2, USAGE));
    };
    let cmd = cmd.to_str().unwrap_or("");
    if matches!(cmd, "-h" | "--help" | "help") {
        out!("{USAGE}");
        return Ok(());
    }
    let args = Args::parse(rest)?;
    match cmd {
        "create" => cmd_create(&args),
        "whoami" => cmd_whoami(&args),
        "list" => cmd_list(&args),
        "integrate" => cmd_integrate(&args),
        "reap" => cmd_reap(&args),
        "adopt" => cmd_adopt(&args),
        "remove" => cmd_remove(&args),
        _ => Err(Fail::new(2, format!("unknown command {cmd:?}\n{USAGE}"))),
    }
}

fn main() {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let result = run(&argv);
    if let Err(fail) = &result {
        let _ = writeln!(std::io::stderr(), "error: {}", fail.msg);
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
