//! Integration strategies for `finish` beyond the default merge: `--squash`
//! and `--rebase`. Both rewrite the task tip, so the original is kept under
//! `refs/wtctl/superseded/<task-id>` before anything changes.

use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Strategy {
    Merge,
    Squash,
    Rebase,
}

pub const REBASE_HOOKS: &[&str] = &[
    "pre-rebase",
    "post-rewrite",
    "post-checkout",
    "commit-msg",
    "post-commit",
    "reference-transaction",
];

pub fn superseded_ref(id: &str) -> String {
    format!("refs/wtctl/superseded/{id}")
}

pub fn parse(a: &Args) -> R<Strategy> {
    match (a.flag("--squash"), a.flag("--rebase")) {
        (true, true) => Err(Fail::new(2, "choose one of --squash and --rebase")),
        (true, false) => Ok(Strategy::Squash),
        (false, true) => Ok(Strategy::Rebase),
        (false, false) => Ok(Strategy::Merge),
    }
}

/// Argument rules shared by `finish` and `finish --dry-run`.
pub fn validate(a: &Args, strategy: Strategy) -> R<()> {
    if strategy == Strategy::Merge {
        return Ok(());
    }
    if a.get("--id")?.is_none() || a.flag("--recover-failed-merge") {
        return Err(Fail::new(
            2,
            "--squash and --rebase need --id and cannot be combined with --recover-failed-merge",
        ));
    }
    if strategy == Strategy::Squash && a.get("--integration-message")?.is_none() {
        return Err(Fail::new(2, "--squash requires --integration-message"));
    }
    Ok(())
}

/// The state both strategies need before they may rewrite history.
fn require_plain_worktree(repo: &Repo, entry: &Entry, id: &str) -> R<()> {
    if repo.uses_annex()? {
        return refuse("--squash and --rebase are not supported in git-annex repositories");
    }
    let facts = task::facts(&[entry], "refs/heads/main")?
        .into_iter()
        .next()
        .unwrap_or_default();
    facts
        .removable_except_integration()
        .map_err(|why| Fail::new(4, format!("{id}: {why}")))?;
    match facts.clean {
        Some(clean) if clean.tracked == 0 && clean.untracked == 0 => Ok(()),
        Some(_) => refuse(format!(
            "{id} has uncommitted or untracked files; commit or remove them before rewriting history"
        )),
        None => refuse(format!("{id} status is unavailable")),
    }
}

pub fn rebase_onto_main(
    repo: &Repo,
    id: &str,
    entry: &Entry,
    main_ref: &str,
    main_tip: &str,
    task_tip: &str,
    allow_hooks: bool,
    allow_signing: bool,
) -> R<String> {
    require_plain_worktree(repo, entry, id)?;
    check_commit_signing(repo, allow_signing)?;
    check_hooks(
        &active_hooks(&entry.path, REBASE_HOOKS, None, None)?,
        allow_hooks,
    )?;
    ensure_ref(&repo.root, main_ref, main_tip)?;
    ensure_task_head(entry, task_tip)?;
    ensure_no_cwd_users(entry, "rebase")?;
    let kept = abandon::preserve_at(
        repo,
        superseded_ref(id),
        task_tip,
        &format!("wtctl: rebase {id}"),
    )?;
    let out = git::output(
        &entry.path,
        ["rebase", "--no-autostash", "--no-stat", main_tip],
    )?;
    if !out.status.success() {
        return refuse(format!(
            "rebase of {id} onto {main_ref} stopped: {}; resolve it in the worktree \
             (git rebase --continue or --abort) and retry; the original tip is kept at {kept}",
            git::stderr_of(&out)
        ));
    }
    let fresh = Entry::load(entry.admin.clone())
        .ok_or_else(|| Fail::new(4, format!("{id} registration disappeared after rebase")))?;
    let tip = fresh
        .current_oid()
        .ok_or_else(|| Fail::new(4, format!("cannot resolve {id} HEAD after rebase")))?;
    if !is_ancestor(&repo.root, main_tip, &tip)? {
        return refuse(format!(
            "{id} is not based on {main_ref} after rebase; original tip kept at {kept}"
        ));
    }
    Ok(tip)
}

pub fn squash_onto_main(
    repo: &Repo,
    id: &str,
    entry: &Entry,
    main_tip: &str,
    task_tip: &str,
    message: &str,
    allow_signing: bool,
) -> R<String> {
    require_plain_worktree(repo, entry, id)?;
    check_commit_signing(repo, allow_signing)?;
    ensure_task_head(entry, task_tip)?;
    ensure_no_cwd_users(entry, "squash")?;
    let signs = git::output(&repo.root, ["config", "--bool", "--get", "commit.gpgsign"])?;
    let signs = signs.status.success() && String::from_utf8_lossy(&signs.stdout).trim() == "true";
    let tree = format!("{task_tip}^{{tree}}");
    let mut args = vec!["commit-tree", &tree, "-p", main_tip, "-m", message];
    if signs {
        args.push("-S");
    }
    let squashed = git::run(&entry.path, &args)?.trim().to_string();
    if !git::is_oid(&squashed) {
        return Err(Fail::new(
            1,
            "git commit-tree returned an unexpected result",
        ));
    }
    abandon::preserve_at(
        repo,
        superseded_ref(id),
        task_tip,
        &format!("wtctl: squash {id}"),
    )?;
    git::run(&entry.path, ["reset", "--soft", &squashed])?;
    let fresh = Entry::load(entry.admin.clone())
        .ok_or_else(|| Fail::new(4, format!("{id} registration disappeared after squash")))?;
    if fresh.current_oid().as_deref() != Some(squashed.as_str()) {
        return refuse(format!("{id} HEAD did not move to the squashed commit"));
    }
    Ok(squashed)
}
