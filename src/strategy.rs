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
    "post-index-change",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "reference-transaction",
];

/// Hooks a merge commit can trigger, shared by the merge path and its preflight.
pub const MERGE_HOOKS: &[&str] = &[
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-merge",
    "post-index-change",
    "reference-transaction",
];

/// Every tip kept for `id`, as `(ref, oid)`. Each rewrite attempt keeps its own
/// ref (`<id>.<n>`), so a retry after the tip changed never has to overwrite one.
fn superseded_refs(repo: &Repo, id: &str) -> Vec<(String, String)> {
    git::run(
        &repo.root,
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &format!("refs/wtctl/superseded/{id}.*"),
        ],
    )
    .map(|text| {
        text.lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(name, oid)| (name.to_string(), oid.to_string()))
            .collect()
    })
    .unwrap_or_default()
}

pub fn kept_tips(repo: &Repo, id: &str) -> Vec<String> {
    superseded_refs(repo, id)
        .into_iter()
        .map(|(_, oid)| oid)
        .collect()
}

fn keep_tip(repo: &Repo, id: &str, tip: &str, message: &str) -> R<String> {
    let existing = superseded_refs(repo, id);
    if let Some((name, _)) = existing.iter().find(|(_, oid)| oid == tip) {
        return Ok(name.clone());
    }
    let refname = format!("refs/wtctl/superseded/{id}.{}", existing.len() + 1);
    abandon::preserve_at(repo, refname, tip, message)
}

/// True when HEAD is already the single squashed commit this id produced on
/// `main_tip`, so a retry must not squash (and keep) a second time.
pub fn already_squashed(repo: &Repo, id: &str, head: &str, main_tip: &str) -> R<bool> {
    let parents = git::run(&repo.root, ["rev-list", "--parents", "-n1", head])?;
    let fields: Vec<&str> = parents.split_whitespace().collect();
    if fields.len() != 2 || fields[1] != main_tip {
        return Ok(false);
    }
    let tree = git::run(&repo.root, ["rev-parse", &format!("{head}^{{tree}}")])?;
    for tip in kept_tips(repo, id) {
        let kept_tree = git::run(&repo.root, ["rev-parse", &format!("{tip}^{{tree}}")])?;
        if kept_tree.trim() == tree.trim() {
            return Ok(true);
        }
    }
    Ok(false)
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
pub fn require_plain_worktree(repo: &Repo, entry: &Entry, id: &str) -> R<()> {
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

pub struct Rebase<'a> {
    pub id: &'a str,
    pub entry: &'a Entry,
    pub main_ref: &'a str,
    pub main_tip: &'a str,
    pub task_tip: &'a str,
    pub allow_hooks: bool,
    pub allow_signing: bool,
}

/// Ignored files survive `git rebase` checkouts silently, so any ignored path
/// that differs between the two tips would be overwritten.
fn refuse_ignored_collisions(r: &Rebase<'_>) -> R<()> {
    let ignored: Vec<Vec<u8>> = extra_status_entries(&r.entry.path)
        .map_err(|why| Fail::new(4, why))?
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    if ignored.is_empty() {
        return Ok(());
    }
    let changed = git::output(
        &r.entry.path,
        [
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            r.task_tip,
            r.main_tip,
        ],
    )?;
    if !changed.status.success() {
        return Err(Fail::new(1, git::stderr_of(&changed)));
    }
    // Replaying commit by commit also touches paths that a later commit reverts.
    let touched = git::output(
        &r.entry.path,
        [
            "log",
            "--format=",
            "--name-only",
            "-z",
            "--no-renames",
            &format!("{}..{}", r.main_tip, r.task_tip),
        ],
    )?;
    if !touched.status.success() {
        return Err(Fail::new(1, git::stderr_of(&touched)));
    }
    let paths = changed
        .stdout
        .split(|b| *b == 0)
        .chain(touched.stdout.split(|b| *b == 0))
        .filter(|p| !p.is_empty());
    for path in paths {
        if ignored.iter().any(|extra| paths_overlap(extra, path)) {
            return refuse(format!(
                "{}: ignored path overlaps {}, which the rebase would overwrite",
                r.id,
                String::from_utf8_lossy(path)
            ));
        }
    }
    Ok(())
}

pub fn rebase_onto_main(repo: &Repo, r: Rebase<'_>) -> R<String> {
    let Rebase {
        id,
        entry,
        main_ref,
        main_tip,
        task_tip,
        allow_hooks,
        allow_signing,
    } = r;
    require_plain_worktree(repo, entry, id)?;
    check_commit_signing(repo, allow_signing)?;
    check_hooks(
        &active_hooks(&entry.path, REBASE_HOOKS, None, None)?,
        allow_hooks,
    )?;
    ensure_ref(&repo.root, main_ref, main_tip)?;
    ensure_task_head(entry, task_tip)?;
    ensure_no_cwd_users(entry, "rebase")?;
    ensure_no_merge_autostash(&entry.path).map_err(|why| Fail::new(4, why))?;
    refuse_ignored_collisions(&Rebase {
        id,
        entry,
        main_ref,
        main_tip,
        task_tip,
        allow_hooks,
        allow_signing,
    })?;
    verify_tracked_filter_safety(repo, &entry.path, &format!("{task_tip}^{{tree}}"), false)?;
    // The checkout of main's files runs their filters too.
    check_active_filters(repo, &entry.path, task_tip, main_tip, false)?;
    let kept = keep_tip(repo, id, task_tip, &format!("wtctl: rebase {id}"))?;
    let out = git::output(
        &entry.path,
        [
            "rebase",
            "--no-autostash",
            "--no-stat",
            "--no-update-refs",
            main_tip,
        ],
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
    keep_tip(repo, id, task_tip, &format!("wtctl: squash {id}"))?;
    // `reset` runs in the worktree, whose own hooks path may differ from the repository's.
    check_hooks(
        &active_hooks(&entry.path, &["reference-transaction"], None, None)?,
        false,
    )?;
    git::run(&entry.path, ["reset", "--soft", &squashed])?;
    let fresh = Entry::load(entry.admin.clone())
        .ok_or_else(|| Fail::new(4, format!("{id} registration disappeared after squash")))?;
    if fresh.current_oid().as_deref() != Some(squashed.as_str()) {
        return refuse(format!("{id} HEAD did not move to the squashed commit"));
    }
    Ok(squashed)
}
