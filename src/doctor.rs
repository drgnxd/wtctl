//! `doctor`: diagnoses stale locks, broken markers, and leftover state. The
//! only repair it performs is releasing a legacy lock whose owner is provably
//! dead on this host.

use super::*;

struct Finding {
    severity: &'static str,
    code: &'static str,
    detail: String,
}

fn finding(severity: &'static str, code: &'static str, detail: impl Into<String>) -> Finding {
    Finding {
        severity,
        code,
        detail: detail.into(),
    }
}

/// `Some(true)` when the pid exists, `Some(false)` when it is gone.
fn pid_exists(pid: i32) -> Option<bool> {
    if pid <= 1 {
        return None;
    }
    // SAFETY: signal 0 only checks existence and permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Some(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Some(false),
        Some(libc::EPERM) => Some(true),
        _ => None,
    }
}

enum LockState {
    Live,
    /// Carries the owner file that was judged dead.
    Stale(String),
    Unknown(String),
}

fn lock_state(dir: &Path) -> LockState {
    let owner = fs::read_to_string(dir.join("owner")).unwrap_or_default();
    let mut lines = owner.lines().skip(1);
    let pid = lines.next().and_then(|line| line.parse::<i32>().ok());
    let host = lines.next().unwrap_or("");
    let Some(pid) = pid else {
        return LockState::Unknown("the owner file is missing or malformed".into());
    };
    if host != lock::hostname() {
        return LockState::Unknown(format!("held from host {host:?}, which cannot be checked"));
    }
    match pid_exists(pid) {
        Some(true) => LockState::Live,
        Some(false) => LockState::Stale(owner),
        None => LockState::Unknown(format!("pid {pid} cannot be inspected")),
    }
}

/// Releases `dir` with the same rename-then-delete protocol a normal release
/// uses. The owner is re-read after the rename: a legacy writer that took the
/// lock over between the check and the rename gets it back untouched.
fn release_stale(common: &Path, dir: &Path, judged: &str) -> Result<(), String> {
    let released = common.join(format!("{}.released.stale-{}", lock::LEGACY_LOCK, now()));
    fs::rename(dir, &released).map_err(|error| error.to_string())?;
    let moved = fs::read_to_string(released.join("owner")).unwrap_or_default();
    if moved != judged {
        return match fs::rename(&released, dir) {
            Ok(()) => Err("the lock changed owner while it was being released; left in place".into()),
            Err(error) => Err(format!(
                "the lock changed owner while it was being released and could not be restored from {}: {error}",
                released.display()
            )),
        };
    }
    let _ = fs::remove_file(released.join("owner"));
    fs::remove_dir(&released).map_err(|error| error.to_string())
}

pub fn cmd_doctor(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let fix = a.flag("--fix-stale-lock");
    if fix && !a.flag("--external-writers-quiesced") {
        return refuse(
            "inspect repository writers, then pass --external-writers-quiesced to release a stale lock",
        );
    }
    let (_, main_ref) = main_ref(a, &repo.root)?;
    let mut findings = Vec::new();
    let mut fixed = Vec::new();

    let lock_dir = repo.common.join(lock::LEGACY_LOCK);
    let leftovers: Vec<PathBuf> = fs::read_dir(&repo.common)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&format!("{}.released.", lock::LEGACY_LOCK)))
        })
        .collect();
    // Holding the flock keeps a concurrent wtctl from acquiring while we repair.
    let (held, lock_error) = match lock::flock(&repo.common.join(lock::FLOCK_PATH)) {
        Ok(held) => (Some(held), None),
        Err(fail) => (None, Some(fail)),
    };
    // A plain diagnosis only probes the lock; repairs keep it for their duration.
    let held = held.filter(|_| fix);
    let can_fix = held.is_some();
    match &lock_error {
        Some(fail) if fail.code == 5 => findings.push(finding(
            "info",
            "lock-held",
            "another wtctl process holds the integration lock right now",
        )),
        Some(fail) => findings.push(finding("error", "lock-unavailable", fail.msg.clone())),
        None => {}
    }
    if lock_dir.is_dir() {
        match lock_state(&lock_dir) {
            LockState::Live => findings.push(finding(
                "info",
                "lock-live",
                format!("{} is held by a live process", lock_dir.display()),
            )),
            LockState::Stale(judged) if can_fix => {
                release_stale(&repo.common, &lock_dir, &judged)
                    .map_err(|error| Fail::new(4, format!("cannot release stale lock: {error}")))?;
                fixed.push(format!("released stale {}", lock_dir.display()));
            }
            LockState::Stale(_) => findings.push(finding(
                "error",
                "lock-stale",
                format!(
                    "{} belongs to a dead process; rerun with --fix-stale-lock --external-writers-quiesced",
                    lock_dir.display()
                ),
            )),
            LockState::Unknown(why) => findings.push(finding(
                "warn",
                "lock-unverified",
                format!("{}: {why}; remove it by hand only after checking", lock_dir.display()),
            )),
        }
    }
    for path in &leftovers {
        if can_fix {
            let _ = fs::remove_file(path.join("owner"));
            match fs::remove_dir(path) {
                Ok(()) => fixed.push(format!("deleted leftover {}", path.display())),
                Err(error) => findings.push(finding(
                    "warn",
                    "lock-leftover",
                    format!("cannot delete {}: {error}", path.display()),
                )),
            }
        } else {
            findings.push(finding(
                "warn",
                "lock-leftover",
                format!(
                    "{} is a leftover from an interrupted release",
                    path.display()
                ),
            ));
        }
    }

    for entry in task::entries(&repo.common) {
        let id = entry.id();
        if let Meta::Invalid(why) = &entry.meta {
            findings.push(finding("error", "marker-invalid", format!("{id}: {why}")));
            continue;
        }
        if !entry.path.is_dir() {
            findings.push(finding(
                "warn",
                "worktree-missing",
                format!(
                    "{id}: registered path {} is gone; `git worktree prune` would drop it{}",
                    entry.path.display(),
                    if entry.lock_reason.is_some() {
                        " after unlocking it"
                    } else {
                        ""
                    }
                ),
            ));
            continue;
        }
        let Some(marker) = entry.marker() else {
            continue;
        };
        let expected = format!("agent-task {}", marker.task_id);
        match entry.lock_reason.as_deref() {
            Some(reason) if reason == expected => {}
            Some(reason) => findings.push(finding(
                "warn",
                "lock-reason",
                format!("{id}: locked with {reason:?}, expected {expected:?}"),
            )),
            None => findings.push(finding(
                "warn",
                "unlocked",
                format!("{id}: the worktree is not locked"),
            )),
        }
        for name in [
            "rebase-merge",
            "rebase-apply",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
        ] {
            if entry.admin.join(name).exists() {
                findings.push(finding(
                    "warn",
                    "in-progress",
                    format!("{id}: {name} is present"),
                ));
            }
        }
        if let Err(why) = artifact::check_pending_transaction(&repo.name(), &entry.path) {
            findings.push(finding(
                "error",
                "pending-transaction",
                format!("{id}: {why}"),
            ));
        }
        if procinfo::liveness(&marker.owner) == Liveness::Dead {
            let unintegrated = task::facts(&[&entry], &main_ref)
                .ok()
                .and_then(|facts| facts.first().and_then(|f| f.unintegrated));
            match unintegrated {
                Some(0) => findings.push(finding(
                    "info",
                    "orphan",
                    format!("{id}: owner is dead and the work is integrated; `wtctl reap` can remove it"),
                )),
                _ => findings.push(finding(
                    "warn",
                    "orphan-unintegrated",
                    format!("{id}: owner is dead and work is not integrated; adopt it or `remove --abandon`"),
                )),
            }
        }
    }

    let count = git::run(
        &repo.root,
        ["for-each-ref", "--format=%(refname)", abandon::PREFIX],
    )
    .map(|text| text.lines().count())
    .unwrap_or(0);
    if count > 0 {
        findings.push(finding(
            "info",
            "kept-refs",
            format!(
                "{count} abandoned task tip(s) are kept under {}",
                abandon::PREFIX
            ),
        ));
    }

    let errors = findings.iter().filter(|f| f.severity == "error").count();
    let warnings = findings.iter().filter(|f| f.severity == "warn").count();
    let status = if errors > 0 {
        "errors"
    } else if warnings > 0 {
        "warnings"
    } else {
        "ok"
    };
    if !fixed.is_empty() {
        repo.audit(json!({"at": now(), "event": "doctor-fix", "fixed": fixed}));
    }
    if a.flag("--json") {
        out!(
            "{}",
            json!({
                "status": status,
                "fixed": fixed,
                "findings": findings.iter().map(|f| json!({
                    "severity": f.severity, "code": f.code, "detail": f.detail,
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        for line in &fixed {
            out!("fixed: {line}");
        }
        for f in &findings {
            out!("[{}] {}: {}", f.severity, f.code, f.detail);
        }
        out!("doctor: {status}");
    }
    if errors > 0 {
        return Err(Fail::new(4, ""));
    }
    Ok(())
}
