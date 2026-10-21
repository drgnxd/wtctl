//! Task markers, registered worktrees, and the facts that gate removal.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;

use serde_json::{json, Value};

use crate::git;
use crate::procinfo::{self, Liveness, Owner};
use crate::Fail;

pub const MARKER: &str = "agent-task.json";
pub const SCHEMA: i64 = 1;
const LEGACY_MARKER: &str = "agent-task";
const LEGACY_TOLERANCE: i64 = 5;
// Bounds concurrent git children (five per worktree) well below common fd limits.
const PARALLEL_WORKTREES: usize = 16;
const IN_PROGRESS: &[&str] = &[
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "BISECT_START",
    "sequencer",
];

pub fn canon(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub fn valid_slug(slug: &str) -> bool {
    let words: Vec<&str> = slug.split('-').collect();
    (2..=4).contains(&words.len())
        && words.iter().all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

pub fn valid_task_id(id: &str) -> bool {
    let parts: Vec<&str> = id.rsplitn(3, '-').collect();
    let [hex, date, slug] = parts[..] else {
        return false;
    };
    hex.len() == 8
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
        && valid_slug(slug)
}

#[derive(Clone, Debug)]
pub struct Marker {
    pub raw: Value,
    pub task_id: String,
    pub worktree: PathBuf,
    pub base: String,
    pub owner: Owner,
}

impl Marker {
    pub fn parse(raw: Value) -> Option<Marker> {
        if raw.get("tool")?.as_str()? != "wtctl" || raw.get("schema")?.as_i64()? != SCHEMA {
            return None;
        }
        let owner = raw.get("owner")?;
        let pid = i32::try_from(owner.get("pid")?.as_i64()?).ok()?;
        let marker = Marker {
            task_id: raw.get("task_id")?.as_str()?.to_string(),
            worktree: PathBuf::from(raw.get("worktree")?.as_str()?),
            base: raw.get("base")?.as_str()?.to_string(),
            owner: Owner {
                pid,
                start: owner.get("start")?.as_str()?.to_string(),
                client: owner.get("client")?.as_str()?.to_string(),
            },
            raw,
        };
        (pid > 1 && valid_task_id(&marker.task_id) && git::is_oid(&marker.base)).then_some(marker)
    }

    pub fn session_hint(&self) -> Option<&str> {
        self.raw
            .get("session_hint")
            .and_then(Value::as_str)
            .filter(|hint| !hint.is_empty())
    }

    /// When ownership was last claimed: creation or the latest adopt. `None`
    /// when the record is missing or malformed, which callers treat as recent.
    pub fn claimed_at(&self) -> Option<i64> {
        match self.raw.get("history") {
            None => {}
            Some(Value::Array(h)) => {
                if let Some(last) = h.last() {
                    return last.get("at")?.as_i64();
                }
            }
            Some(_) => return None,
        }
        self.raw.get("created_unix")?.as_i64()
    }
}

pub fn owner_json(o: &Owner) -> Value {
    json!({"pid": o.pid, "start": o.start, "client": o.client})
}

pub enum Meta {
    Wtctl(Marker),
    Legacy(HashMap<String, String>),
    Invalid(String),
    None,
}

pub fn read_meta(admin: &Path) -> Meta {
    match fs::read(admin.join(MARKER)) {
        Ok(bytes) => {
            return match serde_json::from_slice(&bytes).ok().and_then(Marker::parse) {
                Some(m) => Meta::Wtctl(m),
                None => Meta::Invalid(format!("{} is malformed", admin.join(MARKER).display())),
            }
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Meta::Invalid(e.to_string());
        }
        Err(_) => {}
    }
    match fs::read_to_string(admin.join(LEGACY_MARKER)) {
        Ok(text) => Meta::Legacy(legacy_trailer(&text)),
        Err(_) => Meta::None,
    }
}

/// Parses the legacy `<task> client=.. session=.. base=.. [pid=..] [parent=..]` owner line.
pub fn legacy_trailer(text: &str) -> HashMap<String, String> {
    const KEYS: &[&str] = &["client", "session", "base", "pid", "parent"];
    let mut tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out = HashMap::new();
    while let Some(last) = tokens.last() {
        match last.split_once('=') {
            Some((k, v)) if KEYS.contains(&k) && !v.is_empty() && !out.contains_key(k) => {
                out.insert(k.to_string(), v.to_string());
                tokens.pop();
            }
            _ => break,
        }
    }
    out.insert("task".into(), tokens.join(" "));
    out
}

pub fn legacy_liveness(trailer: &HashMap<String, String>) -> Liveness {
    let parsed = trailer.get("pid").and_then(|v| {
        let (pid, epoch) = v.split_once('@')?;
        Some((pid.parse::<i32>().ok()?, epoch.parse::<i64>().ok()?))
    });
    let Some((pid, epoch)) = parsed.filter(|(pid, _)| *pid > 1) else {
        return Liveness::Unknown;
    };
    judge_legacy(
        procinfo::existence(pid),
        procinfo::is_zombie(pid),
        procinfo::start_epoch(pid),
        epoch,
    )
}

/// Legacy trailers record whole seconds, so the start time matches within a tolerance.
pub fn judge_legacy(
    exists: procinfo::Existence,
    zombie: bool,
    started: Option<i64>,
    recorded: i64,
) -> Liveness {
    use procinfo::Existence::{Missing, Present};
    match (exists, started) {
        (Missing, _) => Liveness::Dead,
        (Present, _) if zombie => Liveness::Dead,
        (_, Some(s)) if (s - recorded).abs() <= LEGACY_TOLERANCE => Liveness::Alive,
        (Present, Some(_)) => Liveness::Dead,
        _ => Liveness::Unknown,
    }
}

pub fn write_marker(admin: &Path, value: &Value) -> Result<(), Fail> {
    let tmp = admin.join(format!("{MARKER}.tmp-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&tmp, admin.join(MARKER))
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Fail::new(
            1,
            format!("cannot write {}: {e}", admin.join(MARKER).display()),
        )
    })
}

pub struct Entry {
    pub admin: PathBuf,
    pub path: PathBuf,
    pub head: String,
    /// Content of the admin `locked` file, when the worktree is locked.
    pub lock_reason: Option<String>,
    pub meta: Meta,
}

fn trim_bytes(b: &[u8]) -> &[u8] {
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    &b[..end]
}

/// Worktree path recorded in an admin dir's `gitdir`, which may be relative to
/// that admin dir (`worktree.useRelativePaths`).
pub fn registered_path(admin: &Path) -> Option<PathBuf> {
    let gitdir = fs::read(admin.join("gitdir")).ok()?;
    let dotgit = Path::new(OsStr::from_bytes(trim_bytes(&gitdir)));
    let dotgit = if dotgit.is_absolute() {
        dotgit.to_path_buf()
    } else {
        admin.join(dotgit)
    };
    Some(canon(dotgit.parent()?))
}

fn has_files(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| match e.file_type() {
        Ok(t) if t.is_dir() => has_files(&e.path()),
        _ => true,
    })
}

impl Entry {
    pub fn load(admin: PathBuf) -> Option<Entry> {
        let path = registered_path(&admin)?;
        let head = fs::read_to_string(admin.join("HEAD"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let lock_reason = fs::read_to_string(admin.join("locked"))
            .ok()
            .map(|s| s.trim().to_string());
        let meta = match read_meta(&admin) {
            Meta::Wtctl(m) if canon(&m.worktree) != path => Meta::Invalid(format!(
                "marker names {} but Git registers {}",
                m.worktree.display(),
                path.display()
            )),
            other => other,
        };
        Some(Entry {
            admin,
            path,
            head,
            lock_reason,
            meta,
        })
    }

    pub fn id(&self) -> String {
        match &self.meta {
            Meta::Wtctl(m) => m.task_id.clone(),
            _ => self
                .admin
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }

    pub fn detached_oid(&self) -> Option<&str> {
        git::is_oid(&self.head).then_some(self.head.as_str())
    }

    pub fn branch_ref(&self) -> Option<&str> {
        self.head
            .strip_prefix("ref: ")
            .filter(|r| r.starts_with("refs/heads/"))
    }

    pub fn current_oid(&self) -> Option<String> {
        if let Some(oid) = self.detached_oid() {
            return Some(oid.to_string());
        }
        let oid = git::run(&self.path, ["rev-parse", "--verify", "HEAD"]).ok()?;
        let oid = oid.trim().to_string();
        git::is_oid(&oid).then_some(oid)
    }

    pub fn marker(&self) -> Option<&Marker> {
        match &self.meta {
            Meta::Wtctl(m) => Some(m),
            _ => None,
        }
    }

    fn in_progress(&self) -> Option<&'static str> {
        IN_PROGRESS
            .iter()
            .copied()
            .find(|name| self.admin.join(name).exists())
    }

    fn per_worktree_state(&self) -> Option<String> {
        // Per-worktree refs and their logs vanish with the admin directory.
        ["refs", "logs/refs"]
            .iter()
            .find(|d| has_files(&self.admin.join(d)))
            .map(|d| format!("per-worktree {d}/ would be lost"))
    }
}

pub fn entries(common: &Path) -> Vec<Entry> {
    let Ok(dir) = fs::read_dir(common.join("worktrees")) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = dir
        .flatten()
        .filter_map(|d| Entry::load(d.path()))
        .collect();
    out.sort_by(|a, b| a.admin.cmp(&b.admin));
    out
}

#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub head: String,
    pub clean: Option<git::Cleanliness>,
    /// Commits reachable from HEAD, its reflog, or per-worktree refs that are not in main.
    pub unintegrated: Option<u64>,
    pub hidden: Option<usize>,
    pub in_progress: Option<&'static str>,
    /// Per-worktree state that removal would lose or cannot account for.
    pub blocker: Option<String>,
}

impl Facts {
    pub fn removable(&self) -> Result<(), String> {
        self.removable_except_status()?;
        let Some(c) = self.clean else {
            return Err("status unavailable".into());
        };
        if !c.is_clean() {
            return Err(format!(
                "not clean ({} tracked, {} untracked, {} ignored)",
                c.tracked, c.untracked, c.ignored
            ));
        }
        Ok(())
    }

    pub fn removable_except_status(&self) -> Result<(), String> {
        self.removable_except_integration()?;
        match self.unintegrated {
            None => Err("integration state unknown".into()),
            Some(n) if n > 0 => Err(format!(
                "{n} commit(s) not in main (HEAD, reflog, or per-worktree refs)"
            )),
            _ => Ok(()),
        }
    }

    pub fn removable_except_integration(&self) -> Result<(), String> {
        if let Some(op) = self.in_progress {
            return Err(format!("{op} in progress"));
        }
        if let Some(why) = &self.blocker {
            return Err(why.clone());
        }
        match self.hidden {
            None => return Err("index flags unavailable".into()),
            Some(n) if n > 0 => return Err(format!("{n} assume-unchanged/skip-worktree entries")),
            _ => {}
        }
        Ok(())
    }

    pub fn removable_with_reflog_opt_in(&self, allow: bool) -> Result<(), String> {
        self.removable_except_status_with_reflog_opt_in(allow)?;
        let Some(clean) = self.clean else {
            return Err("status unavailable".into());
        };
        if !clean.is_clean() {
            return Err(format!(
                "not clean ({} tracked, {} untracked, {} ignored)",
                clean.tracked, clean.untracked, clean.ignored
            ));
        }
        Ok(())
    }

    pub fn removable_except_status_with_reflog_opt_in(&self, allow: bool) -> Result<(), String> {
        self.removable_except_integration()?;
        if allow {
            return Ok(());
        }
        match self.unintegrated {
            None => Err("integration state unknown".into()),
            Some(count) if count > 0 => Err(format!(
                "{count} commit(s) not in main (HEAD, reflog, or per-worktree refs)"
            )),
            _ => Ok(()),
        }
    }
}

struct Probe {
    status: Child,
    ls_files: Child,
    refs: Child,
    reflog: Child,
    reflog_exists: Child,
}

fn collect(child: Child) -> Option<Vec<u8>> {
    let out = child.wait_with_output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn oid_lines(raw: &[u8]) -> Option<Vec<String>> {
    String::from_utf8_lossy(raw)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| git::is_oid(l).then(|| l.to_string()))
        .collect()
}

fn probe(e: &Entry) -> Result<Option<Probe>, Fail> {
    if !e.path.is_dir() {
        return Ok(None);
    }
    Ok(Some(Probe {
        status: git::spawn(&e.path, git::STATUS_ARGS, false)?,
        ls_files: git::spawn(&e.path, &["ls-files", "-v", "-z"], false)?,
        refs: git::spawn(
            &e.path,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/worktree",
                "refs/bisect",
                "refs/rewritten",
            ],
            false,
        )?,
        reflog: git::spawn(
            &e.path,
            &[
                "reflog",
                "show",
                "--no-show-signature",
                "--format=%H",
                "HEAD",
                "--",
            ],
            false,
        )?,
        reflog_exists: git::spawn_status(&e.path, &["reflog", "exists", "HEAD"])?,
    }))
}

/// Everything except the integration count, plus the revisions that count needs.
fn settle(e: &Entry, probe: Option<Probe>) -> (Facts, Option<Vec<String>>) {
    let mut facts = Facts {
        head: e.current_oid().unwrap_or_default(),
        in_progress: e.in_progress(),
        blocker: e.per_worktree_state(),
        ..Facts::default()
    };
    let Some(mut p) = probe else {
        return (facts, None);
    };
    facts.clean = collect(p.status).map(|o| git::parse_status(&o));
    facts.hidden = collect(p.ls_files).map(|o| git::count_hidden(&o));
    let refs = collect(p.refs).and_then(|o| oid_lines(&o));
    let reflog = collect(p.reflog).and_then(|o| oid_lines(&o));
    let exists = p.reflog_exists.wait().map(|s| s.success()).unwrap_or(false);
    if !exists && facts.blocker.is_none() {
        facts.blocker = Some("HEAD reflog unavailable; cannot prove the work is in main".into());
    }
    let revs = match (e.current_oid(), refs, reflog) {
        (Some(head), Some(refs), Some(reflog)) if exists => {
            let mut revs: Vec<String> = std::iter::once(head).chain(refs).chain(reflog).collect();
            revs.sort();
            revs.dedup();
            Some(revs)
        }
        _ => None,
    };
    (facts, revs)
}

/// Status, index flags, and integration for several worktrees, a bounded batch at a time.
pub fn facts(entries: &[&Entry], main_ref: &str) -> Result<Vec<Facts>, Fail> {
    facts_excluding(entries, main_ref, &[])
}

/// Like `facts`, but commits reachable from any `kept` OID also count as
/// preserved (a ref wtctl keeps for a tip it is about to rewrite or drop).
pub fn facts_excluding(
    entries: &[&Entry],
    main_ref: &str,
    kept: &[String],
) -> Result<Vec<Facts>, Fail> {
    let mut out = Vec::with_capacity(entries.len());
    for chunk in entries.chunks(PARALLEL_WORKTREES) {
        let mut probes = Vec::with_capacity(chunk.len());
        for e in chunk {
            probes.push(probe(e)?);
        }
        let settled: Vec<(Facts, Option<Vec<String>>)> = chunk
            .iter()
            .zip(probes)
            .map(|(e, p)| settle(e, p))
            .collect();
        let mut counts = Vec::with_capacity(chunk.len());
        for (e, (_, revs)) in chunk.iter().zip(&settled) {
            counts.push(match revs {
                Some(revs) => {
                    let mut child = git::spawn(&e.path, &["rev-list", "--count", "--stdin"], true)?;
                    let mut input = revs.join("\n");
                    input.push_str(&format!("\n^{main_ref}\n"));
                    for oid in kept {
                        input.push_str(&format!("^{oid}\n"));
                    }
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(input.as_bytes());
                    }
                    Some(child)
                }
                None => None,
            });
        }
        for ((mut facts, _), count) in settled.into_iter().zip(counts) {
            facts.unintegrated = count
                .and_then(collect)
                .and_then(|o| String::from_utf8_lossy(&o).trim().parse().ok());
            out.push(facts);
        }
    }
    Ok(out)
}
