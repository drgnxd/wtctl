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
}

fn index_path(entry: &Entry) -> PathBuf {
    entry.admin.join("wtctl-snapshot.index")
}

/// Tree of everything Git would commit from the worktree (tracked changes plus
/// untracked, non-ignored files), built in a private index so neither the
/// worktree nor its real index is touched.
fn worktree_tree(entry: &Entry) -> R<String> {
    let index = index_path(entry);
    let _ = fs::remove_file(&index);
    let mut results = Vec::new();
    for args in [
        vec!["read-tree", "HEAD"],
        vec!["add", "-A", "--"],
        vec!["write-tree"],
    ] {
        let out = git::command(&entry.path, args)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .map_err(|e| Fail::new(1, format!("cannot run git: {e}")))?;
        if !out.status.success() {
            let _ = fs::remove_file(&index);
            return refuse(format!(
                "cannot snapshot uncommitted work: {}",
                git::stderr_of(&out)
            ));
        }
        results.push(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let _ = fs::remove_file(&index);
    Ok(results.pop().unwrap_or_default())
}

/// Number of paths with uncommitted changes (ignored files excluded).
pub fn uncommitted_paths(entry: &Entry) -> R<usize> {
    let out = git::run(
        &entry.path,
        ["status", "--porcelain", "--untracked-files=all"],
    )?;
    Ok(out.lines().filter(|l| !l.is_empty()).count())
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

/// Captures uncommitted work as a commit whose parent is the task tip.
/// Returns `None` when the worktree is clean.
pub fn snapshot_uncommitted(entry: &Entry, id: &str) -> R<Option<Snapshot>> {
    let head = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, "cannot resolve worktree HEAD"))?;
    let head_tree = git::run(&entry.path, ["rev-parse", &format!("{head}^{{tree}}")])?;
    let tree = worktree_tree(entry)?;
    if tree == head_tree.trim() {
        return Ok(None);
    }
    let message = format!("wtctl: uncommitted changes of {id}");
    let out = git::command(
        &entry.path,
        [
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
    }))
}

/// Resets the worktree to its tip once the snapshot is preserved, so the normal
/// removal checks see a clean tree. Ignored files are left for the artifact
/// policy. Refuses if the worktree changed after the snapshot was taken.
pub fn discard_snapshotted_changes(entry: &Entry, snapshot_tree: &str) -> R<()> {
    if worktree_tree(entry)? != snapshot_tree {
        return refuse("the worktree changed while its snapshot was taken; retry");
    }
    git::run(&entry.path, ["reset", "--hard", "--quiet", "HEAD"])?;
    git::run(&entry.path, ["clean", "-fd", "--quiet"])?;
    if uncommitted_paths(entry)? != 0 {
        return refuse("uncommitted changes remain after the snapshot was discarded");
    }
    Ok(())
}
