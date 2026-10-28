//! Lists dead-owner tasks with the evidence needed to decide whether to
//! `remove --abandon` them: what is unique, what is uncommitted, and how old.

use super::*;

const DEFAULT_DAYS: f64 = 7.0;

fn age_days(entry: &Entry) -> Option<f64> {
    let out = git::output(&entry.path, ["log", "-1", "--format=%ct", "HEAD"]).ok()?;
    let committed: i64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(((now() - committed).max(0) as f64) / 86_400.0)
}

/// Commits on the task that are not patch-equivalent to anything on `main`,
/// so a rebased or cherry-picked copy does not count as unique work.
fn unique_commits(entry: &Entry, main_ref: &str) -> R<Vec<String>> {
    let cherry = git::run(&entry.path, ["cherry", main_ref, "HEAD"])?;
    let mut subjects = Vec::new();
    for line in cherry.lines().filter(|l| l.starts_with('+')) {
        let oid = line[1..].trim();
        let subject = git::run(&entry.path, ["log", "-1", "--format=%s", oid])?;
        subjects.push(subject.trim().to_string());
    }
    Ok(subjects)
}

pub fn cmd_stale(a: &Args) -> R<()> {
    let repo = a.repo(false)?;
    let (_, main_ref) = base_ref(a, &repo.root)?;
    let min_days = match a.get("--older-than-days")? {
        Some(v) => v
            .parse::<f64>()
            .ok()
            .filter(|d| d.is_finite() && *d >= 0.0)
            .ok_or_else(|| Fail::new(2, "--older-than-days must be a non-negative number"))?,
        None => DEFAULT_DAYS,
    };
    let mut rows = Vec::new();
    for entry in task::entries(&repo.common) {
        let Some(marker) = entry.marker() else {
            continue;
        };
        if procinfo::liveness(&marker.owner) != Liveness::Dead {
            continue;
        }
        let Some(days) = age_days(&entry) else {
            continue;
        };
        if days < min_days {
            continue;
        }
        let unique = unique_commits(&entry, &main_ref)?;
        let uncommitted = abandon::uncommitted_paths(&entry)?;
        let ignored = abandon::ignored_paths(&entry)?;
        rows.push(json!({
            "id": entry.id(),
            "path": entry.path,
            "owner": format!("{}:{}", marker.owner.client, marker.owner.pid),
            "idle_days": (days * 10.0).round() / 10.0,
            "unique_commits": unique.len(),
            "unique_subjects": unique.iter().take(5).collect::<Vec<_>>(),
            "uncommitted_paths": uncommitted,
            "ignored_paths": ignored,
            "already_on_main": unique.is_empty() && uncommitted == 0,
        }));
    }
    if a.flag("--json") {
        out!("{}", Value::Array(rows));
        return Ok(());
    }
    for r in &rows {
        out!(
            "{}\tidle={}d\tunique_commits={}\tuncommitted={}\tignored={}\t{}",
            r["id"].as_str().unwrap_or(""),
            r["idle_days"],
            r["unique_commits"],
            r["uncommitted_paths"],
            r["ignored_paths"].as_array().map_or(0, Vec::len),
            if r["already_on_main"] == true {
                "already-on-main"
            } else {
                "has-unique-work"
            }
        );
        for subject in r["unique_subjects"].as_array().into_iter().flatten() {
            out!("    - {}", subject.as_str().unwrap_or(""));
        }
    }
    Ok(())
}
