//! `finish --dry-run`: reports everything a real `finish` would need or trip
//! over, without taking the lock or changing anything.

use super::*;

struct Check {
    name: &'static str,
    state: &'static str,
    detail: String,
    flag: Option<&'static str>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Check {
        Check {
            name,
            state: "ok",
            detail: detail.into(),
            flag: None,
        }
    }

    fn from_result(name: &'static str, result: R<()>, ok_detail: &str) -> Check {
        match result {
            Ok(()) => Check::ok(name, ok_detail),
            Err(fail) => Check {
                name,
                state: "blocked",
                detail: fail.msg,
                flag: None,
            },
        }
    }

    /// A refusal that disappears once `flag` is passed asks for that flag
    /// rather than reporting a hard block.
    fn gated(
        name: &'static str,
        flag: &'static str,
        passed: bool,
        run: impl Fn(bool) -> R<()>,
        ok_detail: &str,
    ) -> Check {
        match run(passed) {
            Ok(()) => Check::ok(name, ok_detail),
            Err(fail) if !passed && run(true).is_ok() => Check {
                name,
                state: "needs-flag",
                detail: fail.msg,
                flag: Some(flag),
            },
            Err(fail) => Check {
                name,
                state: "blocked",
                detail: fail.msg,
                flag: None,
            },
        }
    }
}

fn commit_count(repo: &Repo, range: &str) -> Option<u64> {
    git::run(&repo.root, ["rev-list", "--count", range])
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn cmd_finish_preflight(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    if a.os("--legacy-worktree-id").is_some() || a.os("--expect-branch").is_some() {
        return Err(Fail::new(2, "finish --dry-run supports only --id"));
    }
    let id = a.need("--id")?;
    let keep_worktree = a.flag("--keep-worktree");
    let cleanup = if keep_worktree {
        artifact::Options::default()
    } else {
        artifact_options(a, true)?
    };
    let (main, refname) = main_ref(a, &repo.root)?;
    let (entry, marker) = repo.find_task(id)?;
    let allow_hooks = a.flag("--allow-repository-hooks");
    let allow_filter = a.flag("--allow-annex-filter");
    let allow_signing = a.flag("--allow-commit-signing");

    let mut checks = vec![Check::from_result(
        "owner",
        require_owner(&marker).map(|_| ()),
        "the calling agent owns the task",
    )];
    let main_tip = git::run(&repo.root, ["rev-parse", "--verify", &refname])?
        .trim()
        .to_string();
    let task_tip = entry
        .current_oid()
        .ok_or_else(|| Fail::new(4, format!("cannot resolve {id} HEAD")))?;

    let task_ahead = is_ancestor(&repo.root, &main_tip, &task_tip)?;
    let main_ahead = is_ancestor(&repo.root, &task_tip, &main_tip)?;
    let integration = if main_ahead {
        "already-integrated"
    } else if task_ahead {
        "fast-forward"
    } else {
        "merge"
    };
    let new_commits = commit_count(&repo, &format!("{main_tip}..{task_tip}"));
    let behind = commit_count(&repo, &format!("{task_tip}..{main_tip}"));

    if integration == "merge" {
        checks.push(Check::gated(
            "commit-signing",
            "--allow-commit-signing",
            allow_signing,
            |allow| check_commit_signing(&repo, allow),
            "the integration commit is not configured to sign",
        ));
    }
    if integration == "merge" {
        let preview = git::output(
            &repo.root,
            [
                "merge-tree",
                "--write-tree",
                "--no-messages",
                &main_tip,
                &task_tip,
            ],
        )?;
        checks.push(match preview.status.code() {
            Some(0) => Check::ok("merge-conflicts", "the merge applies cleanly"),
            Some(1) => Check {
                name: "merge-conflicts",
                state: "blocked",
                detail: format!(
                    "merging {main} into the task conflicts; resolve it in the worktree"
                ),
                flag: None,
            },
            _ => Check {
                name: "merge-conflicts",
                state: "blocked",
                detail: format!("merge-tree failed: {}", git::stderr_of(&preview)),
                flag: None,
            },
        });
        checks.push(Check::gated(
            "merge-hooks",
            "--allow-repository-hooks",
            allow_hooks,
            |allow| check_hooks(&active_hooks(&entry.path, MERGE_HOOKS, None, None)?, allow),
            "no repository hook would run while merging",
        ));
        let tree = format!("{task_tip}^{{tree}}");
        checks.push(Check::gated(
            "annex-filters",
            "--allow-annex-filter",
            allow_filter,
            |allow| verify_tracked_filter_safety(&repo, &entry.path, &tree, allow),
            "tracked paths use no unverified filter",
        ));
    } else if integration == "fast-forward" {
        match main_checkout(&repo, &refname)? {
            Some(checkout) => {
                checks.push(Check::gated(
                    "annex-filters",
                    "--allow-annex-filter",
                    allow_filter,
                    |allow| check_active_filters(&repo, &checkout, &main_tip, &task_tip, allow),
                    "no unverified filter applies to the fast-forward",
                ));
                checks.push(Check::gated(
                    "main-hooks",
                    "--allow-repository-hooks",
                    allow_hooks,
                    |allow| {
                        check_hooks(
                            &active_hooks(
                                &checkout,
                                &["post-merge", "reference-transaction"],
                                None,
                                None,
                            )?,
                            allow,
                        )
                    },
                    "no repository hook would run in the main checkout",
                ));
            }
            None => checks.push(Check::ok(
                "main-hooks",
                format!("{main} is not checked out; its ref is updated directly"),
            )),
        }
    }

    let facts = task::facts(&[&entry], &refname)?
        .into_iter()
        .next()
        .unwrap_or_default();
    checks.push(Check::from_result(
        "worktree-state",
        facts
            .removable_except_integration()
            .map_err(|why| Fail::new(4, why)),
        "the worktree is clean enough to remove after integration",
    ));
    let users = procinfo::users_inside(&procinfo::scan_cwds(), &entry.path);
    checks.push(match users {
        Ok(pids) if pids.is_empty() => Check::ok("open-processes", "no process uses the worktree"),
        Ok(pids) => Check {
            name: "open-processes",
            state: "blocked",
            detail: format!(
                "processes still use the worktree (pids: {})",
                pids.join(", ")
            ),
            flag: None,
        },
        Err(why) => Check {
            name: "open-processes",
            state: "blocked",
            detail: format!("cannot inspect processes: {why}"),
            flag: None,
        },
    });
    if !keep_worktree {
        let main_worktree = canonical_main_checkout(&repo)?;
        checks.push(Check::from_result(
            "artifact-cleanup",
            artifact::check_pending_transaction(&repo.name(), &entry.path)
                .and_then(|()| {
                    artifact::inspect_plan(&entry.path, main_worktree.as_deref(), cleanup)
                })
                .map(|_| ())
                .map_err(|why| Fail::new(4, why)),
            "generated artifacts can be cleaned up",
        ));
    }
    if a.flag("--push") {
        checks.push(Check::gated(
            "push",
            "--allow-repository-hooks",
            allow_hooks,
            |allow| remote::check_push(&repo, &remote::remote_arg(a)?, allow),
            "the remote exists and no push hook would run",
        ));
    }
    if repo.uses_annex()? && !a.flag("--external-writers-quiesced") {
        checks.push(Check {
            name: "annex-writers",
            state: "needs-flag",
            detail: "this repository uses git-annex".into(),
            flag: Some("--external-writers-quiesced"),
        });
    }

    let blocked = checks.iter().any(|check| check.state == "blocked");
    let mut required: Vec<&str> = checks.iter().filter_map(|check| check.flag).collect();
    // A real finish always needs the writer acknowledgement.
    if !a.flag("--external-writers-quiesced") && !required.contains(&"--external-writers-quiesced")
    {
        required.push("--external-writers-quiesced");
    }
    let status = if blocked {
        "blocked"
    } else if checks.iter().any(|check| check.state == "needs-flag") {
        "needs-flags"
    } else {
        "ok"
    };
    if a.flag("--json") {
        out!(
            "{}",
            json!({
                "status": status,
                "task_id": id,
                "integration": integration,
                "main_tip": main_tip,
                "task_tip": task_tip,
                "new_commits": new_commits,
                "main_ahead_commits": behind,
                "required_flags": required,
                "checks": checks.iter().map(|check| json!({
                    "name": check.name,
                    "state": check.state,
                    "detail": check.detail,
                    "flag": check.flag,
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        out!(
            "finish preflight for {id}: {status} ({integration}, {} new commit(s), {main} ahead by {})",
            new_commits.map_or("?".into(), |n| n.to_string()),
            behind.map_or("?".into(), |n| n.to_string()),
        );
        for check in &checks {
            out!("  [{}] {}: {}", check.state, check.name, check.detail);
        }
        if !required.is_empty() {
            out!("flags needed for the real run: {}", required.join(" "));
        }
    }
    if status == "ok" {
        Ok(())
    } else {
        Err(Fail::new(4, ""))
    }
}
