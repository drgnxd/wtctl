//! Listing and age-based cleanup of preserved quarantines and kept task tips.

use super::*;

const DAY: i64 = 86_400;

fn quarantine_root(repo: &Repo) -> R<PathBuf> {
    Ok(state_home()?
        .join(repo.name())
        .join("worktrees/.removal-quarantine"))
}

fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| tree_size(&entry.path()))
        .sum()
}

struct Operation {
    id: String,
    created: i64,
    manifest: Value,
    bytes: u64,
}

fn operations(repo: &Repo) -> R<Vec<Operation>> {
    let root = quarantine_root(repo)?;
    let Ok(dir) = fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in dir.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.len() != 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        let manifest = fs::read(&manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or(Value::Null);
        let created = fs::metadata(&manifest_path)
            .or_else(|_| fs::metadata(entry.path()))
            .ok()
            .map_or(0, |meta| meta.ctime());
        found.push(Operation {
            id,
            created,
            manifest,
            bytes: tree_size(&entry.path()),
        });
    }
    found.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    Ok(found)
}

fn preserved_count(manifest: &Value) -> usize {
    manifest["generated_caches"].as_array().map_or(0, |items| {
        items.iter().filter(|item| item["preserve"] == true).count()
    }) + usize::from(manifest["direnv"].is_object())
}

pub fn cmd_quarantine(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let rows: Vec<Value> = operations(&repo)?
        .iter()
        .map(|op| {
            json!({
                "operation_id": op.id,
                "worktree": op.manifest["worktree"],
                "phase": op.manifest["phase"],
                "head": op.manifest["head"],
                "preserved_items": preserved_count(&op.manifest),
                "created_unix": op.created,
                "age_days": (now() - op.created).max(0) / DAY,
                "bytes": op.bytes,
            })
        })
        .collect();
    if a.flag("--json") {
        out!("{}", Value::Array(rows));
    } else {
        for row in &rows {
            out!(
                "{}\t{}d\t{} item(s)\t{} bytes\t{}",
                row["operation_id"].as_str().unwrap_or(""),
                row["age_days"],
                row["preserved_items"],
                row["bytes"],
                row["worktree"].as_str().unwrap_or("?"),
            );
        }
    }
    Ok(())
}

/// Seconds since a kept ref was created, from its first reflog entry.
fn ref_age_secs(repo: &Repo, refname: &str) -> Option<i64> {
    let text = git::run(
        &repo.root,
        [
            "reflog",
            "show",
            "--format=%ct",
            "--end-of-options",
            refname,
        ],
    )
    .ok()?;
    let created = text.lines().last()?.trim().parse::<i64>().ok()?;
    Some((now() - created).max(0))
}

pub fn cmd_prune(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let days = a
        .need("--older-than-days")?
        .parse::<i64>()
        .ok()
        .filter(|days| *days >= 0)
        .ok_or_else(|| Fail::new(2, "--older-than-days must be a non-negative integer"))?;
    let prune_quarantine = a.flag("--quarantine");
    let prune_refs = a.flag("--refs");
    if !prune_quarantine && !prune_refs {
        return Err(Fail::new(2, "prune needs --quarantine and/or --refs"));
    }
    let dry_run = a.flag("--dry-run");
    if !dry_run && !a.flag("--external-writers-quiesced") {
        return refuse("inspect repository writers, then pass --external-writers-quiesced");
    }
    let (_, main_ref) = main_ref(a, &repo.root)?;
    let cutoff = days * DAY;
    let _lock = repo.lock()?;
    let mut actions = Vec::new();

    if prune_quarantine {
        for op in operations(&repo)? {
            if now() - op.created < cutoff {
                continue;
            }
            let outcome = artifact::purge_preserved(
                &repo.root,
                &repo.name(),
                &repo.common,
                &main_ref,
                &op.id,
                false,
                dry_run,
            );
            actions.push(match outcome {
                Ok(labels) => json!({"kind": "quarantine", "id": op.id, "state": if dry_run { "would-purge" } else { "purged" }, "items": labels}),
                Err(why) => json!({"kind": "quarantine", "id": op.id, "state": "skipped", "reason": why}),
            });
        }
    }
    if prune_refs {
        for prefix in [abandon::PREFIX, "refs/wtctl/superseded/"] {
            let listing = git::run(
                &repo.root,
                ["for-each-ref", "--format=%(refname) %(objectname)", prefix],
            )?;
            for line in listing.lines() {
                let Some((refname, oid)) = line.split_once(' ') else {
                    continue;
                };
                let Some(age) = ref_age_secs(&repo, refname) else {
                    actions.push(json!({"kind": "ref", "id": refname, "state": "skipped", "reason": "no creation time in the reflog"}));
                    continue;
                };
                if age < cutoff {
                    continue;
                }
                if dry_run {
                    actions.push(
                        json!({"kind": "ref", "id": refname, "state": "would-delete", "head": oid}),
                    );
                    continue;
                }
                let deleted = git::output(&repo.root, ["update-ref", "-d", refname, oid])?;
                actions.push(if deleted.status.success() {
                    json!({"kind": "ref", "id": refname, "state": "deleted", "head": oid})
                } else {
                    json!({"kind": "ref", "id": refname, "state": "skipped", "reason": git::stderr_of(&deleted)})
                });
            }
        }
    }
    if !dry_run && !actions.is_empty() {
        repo.audit(
            json!({"at": now(), "event": "prune", "older_than_days": days, "actions": actions}),
        );
    }
    if a.flag("--json") {
        out!(
            "{}",
            json!({"status": if dry_run { "dry-run" } else { "pruned" }, "actions": actions})
        );
    } else {
        for action in &actions {
            out!(
                "{}\t{}\t{}{}",
                action["kind"].as_str().unwrap_or(""),
                action["state"].as_str().unwrap_or(""),
                action["id"].as_str().unwrap_or(""),
                action["reason"]
                    .as_str()
                    .map_or(String::new(), |why| format!("\t{why}")),
            );
        }
        if actions.is_empty() {
            out!("nothing older than {days} day(s)");
        }
    }
    Ok(())
}
