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
    preserve_at(repo, ref_name(id), head, &format!("wtctl: abandon {id}"))
}

/// Creates `refname` at `head`. An existing ref must already name `head`, so a
/// retry after a failed step succeeds while a different tip is never overwritten.
pub fn preserve_at(repo: &Repo, refname: String, head: &str, message: &str) -> R<String> {
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
            cas_update(&repo.root, &refname, head, &current, message)?;
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
            message,
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
    let fork = git::output(&repo.root, ["merge-base", "refs/heads/main", &tip])?;
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
