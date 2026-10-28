//! Discarding unintegrated task work without losing it: the task tip is kept
//! under `refs/wtctl/abandoned/<task-id>` and can be restored into a new task.

use super::*;

pub const PREFIX: &str = "refs/wtctl/abandoned/";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

pub fn ref_name(id: &str) -> String {
    format!("{PREFIX}{id}")
}

/// Records `head` under the abandoned namespace.
pub fn preserve(repo: &Repo, id: &str, head: &str) -> R<String> {
    let refname = ref_name(id);
    let message = format!("wtctl: abandon {id}");
    deny_reference_hook(repo)?;
    let existing = git::output(
        &repo.root,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &refname,
        ],
    )?;
    if existing.status.success() {
        let current = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if current == head {
            return Ok(refname);
        }
        // A retry after more commits moves the kept tip forward; it never
        // replaces a tip that the new one does not contain.
        if is_ancestor(&repo.root, &current, head)? {
            cas_update(&repo.root, &refname, head, &current, &message)?;
            return Ok(refname);
        }
        // The ref already keeps more than `head` (a snapshot taken on an earlier
        // attempt whose removal then failed); nothing is lost by keeping it.
        if is_ancestor(&repo.root, head, &current)? {
            return Ok(refname);
        }
        return refuse(format!("{refname} already preserves {current}, not {head}"));
    }
    // The reflog entry records when the tip was kept, which `prune` ages by.
    let out = git::output(
        &repo.root,
        [
            "update-ref",
            "--create-reflog",
            "-m",
            &message,
            &refname,
            head,
            ZERO_OID,
        ],
    )?;
    if !out.status.success() {
        return refuse(format!("cannot create {refname}: {}", git::stderr_of(&out)));
    }
    Ok(refname)
}

pub fn list(a: &Args, repo: &Repo) -> R<()> {
    let format = "%(refname)%09%(objectname)%09%(committerdate:unix)%09%(contents:subject)";
    let text = git::run(
        &repo.root,
        ["for-each-ref", &format!("--format={format}"), PREFIX],
    )?;
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(4, '\t');
        let (Some(name), Some(head), Some(time), Some(subject)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        rows.push(json!({
            "id": name.strip_prefix(PREFIX).unwrap_or(name),
            "ref": name,
            "head": head,
            "committed_unix": time.parse::<i64>().ok(),
            "subject": subject,
        }));
    }
    if a.flag("--json") {
        out!("{}", Value::Array(rows));
    } else {
        for row in &rows {
            out!(
                "{}\t{}\t{}",
                row["id"].as_str().unwrap_or(""),
                row["head"].as_str().unwrap_or(""),
                row["subject"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}

/// Starts a new task worktree from an abandoned tip.
pub fn cmd_restore(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let id = a.need("--id")?;
    if !task::valid_task_id(id) {
        return Err(Fail::new(2, format!("{id:?} is not a wtctl task id")));
    }
    let refname = ref_name(id);
    let found = git::output(
        &repo.root,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &refname,
        ],
    )?;
    if !found.status.success() {
        return Err(Fail::new(
            1,
            format!("no abandoned task {id} (see `wtctl list --abandoned`)"),
        ));
    }
    let mut values: HashMap<String, OsString> = HashMap::new();
    for key in ["--repo", "--slug", "--session", "--worktrees-root"] {
        if let Some(value) = a.os(key) {
            values.insert(key.to_string(), value.to_os_string());
        }
    }
    let task_text = match a.get("--task")? {
        Some(text) => text.to_string(),
        None => format!("restored from abandoned {id}"),
    };
    values.insert("--task".into(), task_text.into());
    values.insert("--from".into(), refname.into());
    values.insert("--parent".into(), id.into());
    // The restored work forked from main at the merge base, so a later default
    // merge finish sees a base that main contains.
    let tip = String::from_utf8_lossy(&found.stdout).trim().to_string();
    let (_, main) = main_ref(a, &repo.root)?;
    let fork = git::output(&repo.root, ["merge-base", &main, &tip])?;
    if fork.status.success() {
        values.insert(
            "--marker-base".into(),
            String::from_utf8_lossy(&fork.stdout)
                .trim()
                .to_string()
                .into(),
        );
    }
    let flags = [
        "--json",
        "--allow-repository-hooks",
        "--external-writers-quiesced",
    ]
    .into_iter()
    .filter(|flag| a.flag(flag))
    .map(str::to_string)
    .collect();
    cmd_create(&Args { values, flags })
}

/// Uncommitted work captured as a commit on top of the task tip.
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    /// Untracked, non-ignored files the snapshot holds; the only files removed
    /// after it is preserved.
    pub untracked: Vec<String>,
}

/// A worktree must have been quiet this long before a dead owner's work is
/// taken: a resumed session in a new process would not show up as the old owner.
const IDLE_SECS: u64 = 600;

/// Git with hooks disabled: a snapshot must not run repository hooks nobody
/// reviewed for this operation.
fn quiet_git(entry: &Entry, args: &[&str]) -> std::process::Command {
    let mut full = vec!["-c", "core.hooksPath=/dev/null"];
    full.extend_from_slice(args);
    git::command(&entry.path, full)
}

struct PrivateIndex(PathBuf);

impl PrivateIndex {
    fn new(entry: &Entry) -> PrivateIndex {
        let path = entry.admin.join("wtctl-snapshot.index");
        let guard = PrivateIndex(path);
        guard.clear();
        guard
    }

    fn clear(&self) {
        let _ = fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(PathBuf::from(lock));
    }
}

impl Drop for PrivateIndex {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Tree of everything Git would commit from the worktree (tracked changes plus
/// untracked, non-ignored files), built in a private index so neither the
/// worktree nor its real index is touched.
fn worktree_tree(entry: &Entry) -> R<String> {
    let index = PrivateIndex::new(entry);
    let mut last = String::new();
    for args in [
        vec!["read-tree", "HEAD"],
        vec!["add", "-A", "--"],
        vec!["write-tree"],
    ] {
        let out = quiet_git(entry, &args)
            .env("GIT_INDEX_FILE", &index.0)
            .output()
            .map_err(|e| Fail::new(1, format!("cannot run git: {e}")))?;
        if !out.status.success() {
            return refuse(format!(
                "cannot snapshot uncommitted work: {}",
                git::stderr_of(&out)
            ));
        }
        last = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    Ok(last)
}

/// Paths with uncommitted changes (ignored files excluded), as reported by
/// `git status`. Renames and copies yield their new path.
fn changed_paths(entry: &Entry) -> R<Vec<String>> {
    let out = git::output(
        &entry.path,
        ["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;
    if !out.status.success() {
        return refuse(format!("git status failed: {}", git::stderr_of(&out)));
    }
    let mut fields = out
        .stdout
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned());
    let mut paths = Vec::new();
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        if matches!(field.as_bytes()[0], b'R' | b'C') || matches!(field.as_bytes()[1], b'R' | b'C')
        {
            fields.next();
        }
        paths.push(field[3..].to_string());
    }
    Ok(paths)
}

/// Number of paths with uncommitted changes (ignored files excluded).
pub fn uncommitted_paths(entry: &Entry) -> R<usize> {
    Ok(changed_paths(entry)?.len())
}

/// Ignored paths, which a normal removal deletes with the worktree and which a
/// snapshot does not keep. Reported so the caller can see what would be lost.
pub fn ignored_paths(entry: &Entry) -> R<Vec<String>> {
    let out = git::run(
        &entry.path,
        [
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=normal",
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("!! ").map(str::to_string))
        .collect())
}

fn untracked_files(entry: &Entry) -> R<Vec<String>> {
    let out = git::output(
        &entry.path,
        ["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    if !out.status.success() {
        return refuse(format!("git ls-files failed: {}", git::stderr_of(&out)));
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect())
}

/// Seconds since the newest change among the uncommitted files, if any.
pub fn newest_change_age_secs(entry: &Entry) -> R<Option<u64>> {
    let now = std::time::SystemTime::now();
    let mut newest: Option<u64> = None;
    for path in changed_paths(entry)? {
        let Ok(meta) = fs::symlink_metadata(entry.path.join(&path)) else {
            continue;
        };
        let Ok(age) = meta.modified().and_then(|m| {
            now.duration_since(m)
                .map_err(|e| std::io::Error::other(e.to_string()))
        }) else {
            continue;
        };
        newest = Some(newest.map_or(age.as_secs(), |n| n.min(age.as_secs())));
    }
    Ok(newest)
}

/// Every refusal that `remove --abandon --include-uncommitted` can predict is
/// raised here, before anything is written or reset, so that a refusal never
/// follows the point of no return.
pub fn preflight_snapshot(
    repo: &Repo,
    entry: &Entry,
    annex: bool,
    dead_owner: bool,
    ignored_ok: bool,
) -> R<()> {
    let id = entry.id();
    deny_reference_hook(repo)?;
    let head = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD"))?;
    if entry.admin.join("modules").exists() {
        return refuse(format!(
            "{id} contains submodule state; remove it separately"
        ));
    }
    let index = git::output(&entry.path, ["ls-files", "--stage", "-z"])?;
    if !index.status.success()
        || index.stdout.split(|b| *b == 0).any(|record| {
            record
                .split(|b| *b == b'\t')
                .next()
                .is_some_and(|meta| meta.starts_with(b"160000 "))
        })
    {
        return refuse(format!(
            "{id} contains submodules or its index cannot be inspected"
        ));
    }
    let expected_tree = format!("{head}^{{tree}}");
    verify_tracked_filter_safety(repo, &entry.path, &expected_tree, annex)?;
    let untracked = untracked_files(entry)?;
    if untracked.iter().any(|p| p.ends_with('/')) {
        return refuse(format!(
            "{id} contains a nested repository; remove or commit it first"
        ));
    }
    if !untracked.is_empty() {
        let bytes: Vec<Vec<u8>> = untracked.iter().map(|p| p.as_bytes().to_vec()).collect();
        verify_filter_paths(
            repo,
            &entry.path,
            &bytes,
            &expected_tree,
            annex,
            "untracked paths",
        )?;
    }
    for path in changed_paths(entry)? {
        let name = Path::new(&path).file_name().and_then(|n| n.to_str());
        if matches!(name, Some(".gitignore" | ".gitattributes")) {
            return refuse(format!(
                "{path} changed: ignore and attribute rules would differ between the snapshot and the reset; commit or revert it first"
            ));
        }
    }
    gitfile_pointer(entry)?;
    let refname = ref_name(&id);
    let existing = git::output(
        &repo.root,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &refname,
        ],
    )?;
    if existing.status.success() {
        let current = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        let fits = current == head
            || is_ancestor(&repo.root, &current, &head)?
            || is_ancestor(&repo.root, &head, &current)?;
        if !fits {
            return refuse(format!("{refname} already preserves {current}, not {head}"));
        }
    }
    if dead_owner && !ignored_ok {
        let ignored = ignored_paths(entry)?;
        if !ignored.is_empty() {
            return refuse(format!(
                "{id}: ignored files are not kept and would be deleted with the worktree: {}; pass --discard-ignored-files to accept that for a dead owner's task",
                ignored.join(", ")
            ));
        }
    }
    if dead_owner {
        if let Some(age) = newest_change_age_secs(entry)? {
            if age < IDLE_SECS {
                return refuse(format!(
                    "{id}: uncommitted files changed {age}s ago; the owner may have been resumed in another process, so a dead owner's work is taken only after {IDLE_SECS}s of quiet"
                ));
            }
        }
    }
    Ok(())
}

/// Staged work must appear unchanged in the snapshot, or it would exist only as
/// a loose object once the index is reset. Read-only: it compares `git
/// diff-index --cached` entries against the snapshot tree path by path.
fn ensure_staged_matches(entry: &Entry, tree: &str) -> R<()> {
    let raw = git::output(
        &entry.path,
        [
            "diff-index",
            "--cached",
            "--raw",
            "-z",
            "--no-renames",
            "HEAD",
        ],
    )?;
    if !raw.status.success() {
        return refuse(format!(
            "cannot compare the index: {}",
            git::stderr_of(&raw)
        ));
    }
    let mut fields = raw.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(meta) = fields.next() {
        let Some(path) = fields.next() else { break };
        let meta = String::from_utf8_lossy(meta);
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (Some(mode), Some(oid)) = (parts.get(1), parts.get(3)) else {
            continue;
        };
        let path = String::from_utf8_lossy(path).into_owned();
        let in_tree = git::run(&entry.path, ["ls-tree", "-z", tree, "--", &path])?;
        let entry_line = in_tree.trim_end_matches(' ');
        let matches = if *mode == "000000" {
            entry_line.is_empty()
        } else {
            entry_line.starts_with(&format!("{mode} ")) && entry_line.contains(&format!(" {oid}	"))
        };
        if !matches {
            return refuse(format!(
                "{path} is staged differently from the worktree; commit or reset it first"
            ));
        }
    }
    Ok(())
}

/// Refuses content that Git stores outside the snapshot commit: annex pointers
/// and symlinks into the annex, and gitlinks.
fn ensure_self_contained(entry: &Entry, head: &str, tree: &str) -> R<()> {
    let raw = git::output(&entry.path, ["diff-tree", "-r", "-z", "--raw", head, tree])?;
    if !raw.status.success() {
        return refuse(format!(
            "cannot inspect the snapshot: {}",
            git::stderr_of(&raw)
        ));
    }
    let mut fields = raw.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(meta) = fields.next() {
        let _path = fields.next();
        let meta = String::from_utf8_lossy(meta);
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (Some(mode), Some(oid)) = (parts.get(1), parts.get(3)) else {
            continue;
        };
        if *mode == "160000" {
            return refuse("the snapshot would contain a gitlink");
        }
        if *mode == "000000" {
            continue;
        }
        let size = git::run(&entry.path, ["cat-file", "-s", oid])?;
        if *mode == "120000" || size.trim().parse::<u64>().unwrap_or(u64::MAX) <= 1024 {
            let body = git::run(&entry.path, ["cat-file", "-p", oid])?;
            if body.contains("/annex/objects/") {
                return refuse(
                    "a changed path is stored in git-annex; its content would live outside the snapshot",
                );
            }
        }
    }
    Ok(())
}

/// Captures uncommitted work as a commit whose parent is the task tip.
/// Returns `None` when the worktree is clean.
pub fn snapshot_uncommitted(entry: &Entry, id: &str) -> R<Option<Snapshot>> {
    let head = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD"))?;
    let head_tree = git::run(&entry.path, ["rev-parse", &format!("{head}^{{tree}}")])?;
    let head_tree = head_tree.trim().to_string();
    let untracked = untracked_files(entry)?;
    let tree = worktree_tree(entry)?;
    if tree == head_tree {
        return Ok(None);
    }
    ensure_staged_matches(entry, &tree)?;
    ensure_self_contained(entry, &head, &tree)?;
    let message = format!("wtctl: uncommitted changes of {id}");
    let out = quiet_git(
        entry,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            &message,
        ],
    )
    .env("GIT_AUTHOR_NAME", "wtctl")
    .env("GIT_AUTHOR_EMAIL", "wtctl@localhost")
    .env("GIT_COMMITTER_NAME", "wtctl")
    .env("GIT_COMMITTER_EMAIL", "wtctl@localhost")
    .output()
    .map_err(|e| Fail::new(1, format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return refuse(format!(
            "cannot record the snapshot: {}",
            git::stderr_of(&out)
        ));
    }
    Ok(Some(Snapshot {
        commit: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        tree,
        untracked,
    }))
}

/// Refusals before the worktree is changed, and failures after it was.
pub enum ResetError {
    Before(Fail),
    After(Fail),
}

/// Checked before the ref is written: nothing has changed yet.
pub fn ensure_still_snapshotted(entry: &Entry, snapshot: &Snapshot) -> R<()> {
    ensure_no_cwd_users(entry, "while its uncommitted work is discarded")?;
    if worktree_tree(entry)? != snapshot.tree {
        return refuse("the worktree changed while its snapshot was taken; retry");
    }
    Ok(())
}

/// A deletion must never follow a symlink that replaced a directory.
fn remove_untracked_file(root: &Path, relative: &str) -> R<()> {
    let mut current = root.to_path_buf();
    let parent = Path::new(relative).parent().unwrap_or(Path::new(""));
    for component in parent.components() {
        current.push(component);
        let meta = fs::symlink_metadata(&current)
            .map_err(|e| Fail::new(4, format!("{}: {e}", current.display())))?;
        if !meta.is_dir() {
            return refuse(format!(
                "a parent of {relative} is no longer a plain directory; left in place"
            ));
        }
    }
    let full = root.join(relative);
    match fs::symlink_metadata(&full) {
        Ok(meta) if meta.is_dir() => {
            return refuse(format!("{relative} is a directory now; left in place"))
        }
        Ok(_) => fs::remove_file(&full)
            .map_err(|e| Fail::new(4, format!("cannot remove {relative}: {e}")))?,
        Err(_) => {}
    }
    let mut dir = full.parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if d == root || fs::remove_dir(&d).is_err() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    Ok(())
}

/// Resets the worktree to its tip once the snapshot is preserved, so the normal
/// removal checks see a clean tree. Only files the snapshot holds are deleted;
/// ignored files are left for the artifact policy.
pub fn reset_and_clean(entry: &Entry, snapshot: &Snapshot) -> Result<(), ResetError> {
    ensure_still_snapshotted(entry, snapshot).map_err(ResetError::Before)?;
    let reset = quiet_git(entry, &["reset", "--hard", "--quiet", "HEAD"])
        .output()
        .map_err(|e| ResetError::Before(Fail::new(1, format!("cannot run git: {e}"))))?;
    if !reset.status.success() {
        return Err(ResetError::After(Fail::new(
            4,
            format!("git reset failed: {}", git::stderr_of(&reset)),
        )));
    }
    for path in &snapshot.untracked {
        let probe = git::output(
            &entry.path,
            ["cat-file", "-e", &format!("{}:{path}", snapshot.tree)],
        )
        .map_err(ResetError::After)?;
        if !probe.status.success() {
            return Err(ResetError::After(Fail::new(
                4,
                format!("{path} is not in the snapshot; left in place"),
            )));
        }
        remove_untracked_file(&entry.path, path).map_err(ResetError::After)?;
    }
    if uncommitted_paths(entry).map_err(ResetError::After)? != 0 {
        return Err(ResetError::After(Fail::new(
            4,
            "uncommitted changes remain after the snapshot was discarded",
        )));
    }
    Ok(())
}

/// Puts the abandoned ref back as it was before this run wrote it.
pub fn rollback_ref(repo: &Repo, id: &str, written: &str, previous: Option<&str>) {
    let refname = ref_name(id);
    let _ = match previous {
        Some(old) => git::output(&repo.root, ["update-ref", &refname, old, written]),
        None => git::output(&repo.root, ["update-ref", "-d", &refname, written]),
    };
}

/// Current value of the abandoned ref, if any.
pub fn current_ref(repo: &Repo, id: &str) -> R<Option<String>> {
    let out = git::output(
        &repo.root,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &ref_name(id),
        ],
    )?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}
