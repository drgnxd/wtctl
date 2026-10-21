use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::git::{self, StatusEntry};
use serde_json::{json, Value};

#[path = "artifact_config.rs"]
mod artifact_config;

const PRECOMMIT_CONFIG: &str = ".pre-commit-config.yaml";
const MAX_PRECOMMIT_BYTES: u64 = 16 * 1024 * 1024;
const PURGE_BATCH_ENTRIES: usize = 256;
const LSOF_BATCH_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    pub relative: String,
    pub kind: String,
    pub device: u64,
    pub inode: u64,
    pub mode: u32,
    pub size: u64,
    pub mtime_ns: i128,
    pub link_or_digest: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub discard_direnv: bool,
    pub discard_caches: bool,
    pub discard_build_outputs: bool,
    pub discard_all: bool,
    pub preserve_generated: bool,
    pub allow_unreferenced_reflog: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignedArtifact {
    pub path: PathBuf,
    pub signature: Vec<TreeEntry>,
    pub preserve: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseLink {
    pub path: PathBuf,
    pub link_text: PathBuf,
    pub target: PathBuf,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Plan {
    pub generated: Vec<SignedArtifact>,
    pub direnv: Option<SignedArtifact>,
    pub direnv_discardable: bool,
    pub discard_direnv: bool,
    pub database_link: Option<DatabaseLink>,
}

#[derive(Clone, Debug)]
pub struct MovedArtifact {
    pub original: PathBuf,
    pub quarantine: PathBuf,
    pub journal: PathBuf,
    pub signature: Vec<TreeEntry>,
    pub preserve: bool,
    pub direnv: bool,
}

#[derive(Clone, Debug)]
pub struct Transaction {
    pub operation: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Value,
    pub moved: Vec<MovedArtifact>,
    pub database_link: Option<DatabaseLink>,
}

struct BeginTransactionContext<'a> {
    repo_name: &'a str,
    state_home: &'a Path,
    common: &'a Path,
    worktree: &'a Path,
    admin: &'a Path,
    head: &'a str,
    branch: Option<&'a str>,
}

struct PurgeContext<'a> {
    state_home: &'a Path,
    repo_root: &'a Path,
    repo_name: &'a str,
    common: &'a Path,
    main_ref: &'a str,
    operation_id: &'a str,
    discard_unverified_direnv: bool,
    dry_run: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FinishReport {
    pub preserved: Vec<PathBuf>,
    pub discarded: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub operation: Option<PathBuf>,
}

pub fn begin_transaction(
    plan: &Plan,
    repo_name: &str,
    common: &Path,
    worktree: &Path,
    admin: &Path,
    head: &str,
    branch: Option<&str>,
) -> Result<Option<Transaction>, String> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    begin_transaction_in_state(
        plan,
        BeginTransactionContext {
            repo_name,
            state_home: &state_home,
            common,
            worktree,
            admin,
            head,
            branch,
        },
    )
}

fn begin_transaction_in_state(
    plan: &Plan,
    context: BeginTransactionContext<'_>,
) -> Result<Option<Transaction>, String> {
    let BeginTransactionContext {
        repo_name,
        state_home,
        common,
        worktree,
        admin,
        head,
        branch,
    } = context;
    if plan.generated.is_empty() && plan.direnv.is_none() && plan.database_link.is_none() {
        return Ok(None);
    }
    if !state_home.is_absolute() {
        return Err("XDG_STATE_HOME must be an absolute path for worktree quarantine".into());
    }
    let root = state_home
        .join(repo_name)
        .join("worktrees/.removal-quarantine");
    fs::create_dir_all(root.parent().unwrap())
        .map_err(|error| format!("cannot create quarantine parent: {error}"))?;
    let mut quarantine_builder = fs::DirBuilder::new();
    quarantine_builder.mode(0o700);
    match quarantine_builder.create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "cannot create quarantine root {}: {error}",
                root.display()
            ))
        }
    }
    let root_real = fs::canonicalize(&root)
        .map_err(|error| format!("cannot resolve quarantine root: {error}"))?;
    let repository = common.parent().unwrap_or(common);
    if root_real == worktree
        || root_real.starts_with(worktree)
        || root_real == admin
        || root_real == repository
        || root_real.starts_with(repository)
    {
        return Err(format!(
            "quarantine root would overlap the repository or worktree: {}",
            root.display()
        ));
    }
    let root_meta = fs::symlink_metadata(&root)
        .map_err(|error| format!("cannot inspect quarantine root {}: {error}", root.display()))?;
    if !root_meta.is_dir()
        || root_meta.file_type().is_symlink()
        || root_meta.uid() != unsafe { libc::getuid() }
        || root_meta.mode() & 0o077 != 0
    {
        return Err(format!(
            "quarantine root is not a private directory owned by the current user: {}",
            root.display()
        ));
    }

    let mut moved_plan = plan.generated.clone();
    if let Some(direnv) = &plan.direnv {
        moved_plan.push(direnv.clone());
    }
    for artifact in &moved_plan {
        let meta = fs::symlink_metadata(&artifact.path).map_err(|error| {
            format!(
                "cannot inspect artifact {}: {error}",
                artifact.path.display()
            )
        })?;
        if meta.dev() != root_meta.dev() {
            return Err(format!(
                "quarantine root is on another filesystem from {}",
                artifact.path.display()
            ));
        }
    }

    let operation = create_operation_dir(&root)?;
    let manifest_path = operation.join("manifest.json");
    let worktree_meta = fs::symlink_metadata(worktree)
        .map_err(|error| format!("cannot inspect worktree {}: {error}", worktree.display()))?;
    let admin_meta = fs::symlink_metadata(admin).map_err(|error| {
        format!(
            "cannot inspect admin directory {}: {error}",
            admin.display()
        )
    })?;
    let generated_manifest: Vec<Value> = plan
        .generated
        .iter()
        .enumerate()
        .map(|(index, item)| {
            json!({
                "path": item.path.strip_prefix(worktree).unwrap().to_string_lossy(),
                "signature": signature_json(&item.signature),
                "quarantine_path": operation.join(format!("generated-cache-{index:03}")),
                "purge_journal": format!("purge-generated-cache-{index:03}.jsonl"),
                "preserve": item.preserve,
                "purge_state": "planned",
            })
        })
        .collect();
    let direnv = plan.direnv.as_ref().map(|item| {
        json!({
            "path": ".direnv",
            "signature": signature_json(&item.signature),
            "identity": [item.signature[0].device, item.signature[0].inode],
            "quarantine_path": operation.join(".direnv"),
            "purge_journal": "purge-direnv.jsonl",
            "discard_requested": plan.discard_direnv,
            "discardable": plan.direnv_discardable,
            "purge_state": "planned",
        })
    });
    let database = plan.database_link.as_ref().map(|link| {
        json!({
            "link_text": link.link_text,
            "target": link.target,
            "device": link.device,
            "inode": link.inode,
            "unlinked": false,
        })
    });
    let manifest = json!({
        "schema_version": 1,
        "operation_id": operation.file_name().unwrap_or_default().to_string_lossy(),
        "phase": "prepared",
        "worktree": worktree,
        "worktree_identity": [worktree_meta.dev(), worktree_meta.ino()],
        "admin_dir": admin,
        "admin_identity": [admin_meta.dev(), admin_meta.ino()],
        "common_dir": common,
        "branch_ref": branch,
        "head": head,
        "database_link": database,
        "direnv": direnv,
        "generated_caches": generated_manifest,
    });
    let transaction = Transaction {
        operation,
        manifest_path,
        manifest,
        moved: Vec::new(),
        database_link: plan.database_link.clone(),
    };
    if let Err(error) = write_manifest(&transaction, "prepared", true) {
        if fs::symlink_metadata(&transaction.manifest_path).is_err()
            && fs::read_dir(&transaction.operation)
                .is_ok_and(|mut entries| entries.next().is_none())
        {
            let _ = fs::remove_dir(&transaction.operation);
            let _ = sync_dir(&root);
        }
        return Err(error);
    }
    Ok(Some(transaction))
}

pub fn quarantine(
    transaction: &mut Transaction,
    plan: &Plan,
    worktree: &Path,
) -> Result<(), String> {
    if let Some(direnv) = &plan.direnv {
        move_artifact(transaction, direnv, true, worktree)?;
    }
    for artifact in &plan.generated {
        move_artifact(transaction, artifact, false, worktree)?;
    }
    if let Some(link) = &plan.database_link {
        verify_database_link(link)?;
        fs::remove_file(&link.path).map_err(|error| {
            format!(
                "cannot unlink canonical state.db symlink {}: {error}",
                link.path.display()
            )
        })?;
        transaction.manifest["database_link"]["unlinked"] = Value::Bool(true);
        write_manifest(transaction, "database_symlink_unlinked", false)?;
    }
    Ok(())
}

fn move_artifact(
    transaction: &mut Transaction,
    artifact: &SignedArtifact,
    direnv: bool,
    worktree: &Path,
) -> Result<(), String> {
    verify_signature(&artifact.path, &artifact.signature, worktree)?;
    let open_paths: Vec<_> = artifact
        .signature
        .iter()
        .filter(|entry| entry.kind != "symlink" && entry.kind != "special")
        .map(|entry| {
            if entry.relative == "." {
                artifact.path.clone()
            } else {
                artifact.path.join(&entry.relative)
            }
        })
        .collect();
    let open = lsof_open_paths(&open_paths)?;
    if !open.is_empty() {
        return Err(format!(
            "generated artifact opened during removal by process(es): {}",
            open.join(", ")
        ));
    }
    let name = if direnv {
        ".direnv".to_string()
    } else {
        let index = transaction.moved.iter().filter(|item| !item.direnv).count();
        format!("generated-cache-{index:03}")
    };
    let destination = transaction.operation.join(name);
    let journal = transaction.operation.join(if direnv {
        "purge-direnv.jsonl".to_string()
    } else {
        format!(
            "purge-{}.jsonl",
            destination.file_name().unwrap().to_string_lossy()
        )
    });
    transaction.moved.push(MovedArtifact {
        original: artifact.path.clone(),
        quarantine: destination.clone(),
        journal,
        signature: artifact.signature.clone(),
        preserve: artifact.preserve,
        direnv,
    });
    write_manifest(transaction, "artifact_move_started", false)?;
    rename_noreplace(&artifact.path, &destination)?;
    sync_dir(artifact.path.parent().unwrap_or(worktree))?;
    sync_dir(&transaction.operation)?;
    verify_signature(&destination, &artifact.signature, worktree)?;
    write_manifest(transaction, "artifact_quarantined", false)
}

pub fn restore(
    transaction: &mut Transaction,
    worktree: &Path,
    admin: &Path,
    head: &str,
) -> Result<(), String> {
    verify_transaction_identity(transaction, worktree, admin, head)?;
    if let Some(link) = &transaction.database_link {
        if transaction.manifest["database_link"]["unlinked"] == true {
            restore_database_link(link)?;
            transaction.manifest["database_link"]["unlinked"] = Value::Bool(false);
        }
    }
    for moved in transaction.moved.iter().rev() {
        if fs::symlink_metadata(&moved.quarantine).is_err() {
            verify_signature(&moved.original, &moved.signature, worktree)?;
            continue;
        }
        if fs::symlink_metadata(&moved.original).is_ok() {
            return Err(format!(
                "artifact restore path is occupied: {}",
                moved.original.display()
            ));
        }
        verify_signature(&moved.quarantine, &moved.signature, worktree)?;
        rename_noreplace(&moved.quarantine, &moved.original)?;
        sync_dir(moved.original.parent().unwrap_or(worktree))?;
    }
    write_manifest(transaction, "rollback_completed", false)?;
    remove_transaction_dir(transaction)
}

pub fn finish_transaction(transaction: &mut Transaction) -> Result<FinishReport, String> {
    let mut report = FinishReport {
        operation: Some(transaction.operation.clone()),
        ..FinishReport::default()
    };
    for index in 0..transaction.moved.len() {
        let moved = transaction.moved[index].clone();
        let record = transaction_record_mut(transaction, &moved)?;
        if moved.preserve {
            record["purge_state"] = Value::String("retained".into());
            record["preserve_reason"] =
                Value::String("explicitly retained generated artifact".into());
            write_manifest(transaction, "completed_preserved", false)?;
            report.preserved.push(moved.quarantine);
            continue;
        }
        record["purge_state"] = Value::String("purging".into());
        write_manifest(transaction, "purging_preserved", false)?;
        match purge_tree(transaction, &moved) {
            Ok(()) => {
                let record = transaction_record_mut(transaction, &moved)?;
                record["purge_state"] = Value::String("purged".into());
                record.as_object_mut().unwrap().remove("purge_cursor");
                record.as_object_mut().unwrap().remove("purge_pending");
                record.as_object_mut().unwrap().remove("purge_warning");
                write_manifest(transaction, "purging_preserved", false)?;
                report.discarded.push(moved.original);
            }
            Err(reason) => {
                let record = transaction_record_mut(transaction, &moved)?;
                record["purge_warning"] = Value::String(reason.clone());
                write_manifest(transaction, "completed_preserved", false)?;
                report.preserved.push(moved.quarantine);
                report.warnings.push(reason);
            }
        }
    }
    if report.preserved.is_empty() {
        transaction.manifest["phase"] = Value::String("worktree_removed".into());
        write_manifest(transaction, "worktree_removed", false)?;
        remove_transaction_dir(transaction)?;
        report.operation = None;
    } else {
        transaction.manifest["phase"] = Value::String("completed_preserved".into());
        write_manifest(transaction, "completed_preserved", false)?;
    }
    Ok(report)
}

pub fn purge_preserved(
    repo_root: &Path,
    repo_name: &str,
    common: &Path,
    main_ref: &str,
    operation_id: &str,
    discard_unverified_direnv: bool,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    purge_preserved_in_state(PurgeContext {
        state_home: &state_home,
        repo_root,
        repo_name,
        common,
        main_ref,
        operation_id,
        discard_unverified_direnv,
        dry_run,
    })
}

fn purge_preserved_in_state(context: PurgeContext<'_>) -> Result<Vec<String>, String> {
    let PurgeContext {
        state_home,
        repo_root,
        repo_name,
        common,
        main_ref,
        operation_id,
        discard_unverified_direnv,
        dry_run,
    } = context;
    if operation_id.len() != 32
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("quarantine operation ID must be 32 lowercase hexadecimal characters".into());
    }
    if !state_home.is_absolute() {
        return Err("XDG_STATE_HOME must be absolute for quarantine cleanup".into());
    }
    let root = state_home
        .join(repo_name)
        .join("worktrees/.removal-quarantine");
    let operation = root.join(operation_id);
    let root_meta = fs::symlink_metadata(&root)
        .map_err(|error| format!("quarantine root does not exist: {error}"))?;
    let operation_meta = fs::symlink_metadata(&operation)
        .map_err(|error| format!("preserved quarantine transaction not found: {error}"))?;
    if !private_dir(&root_meta)
        || !private_dir(&operation_meta)
        || root_meta.dev() != operation_meta.dev()
    {
        return Err(
            "quarantine root or operation is not a private directory owned by the current user"
                .into(),
        );
    }
    let manifest_path = operation.join("manifest.json");
    let manifest_meta = fs::symlink_metadata(&manifest_path)
        .map_err(|error| format!("cannot inspect quarantine manifest: {error}"))?;
    if !manifest_meta.is_file()
        || manifest_meta.file_type().is_symlink()
        || manifest_meta.uid() != unsafe { libc::getuid() }
        || manifest_meta.mode() & 0o077 != 0
        || manifest_meta.size() > 64 * 1024 * 1024
    {
        return Err("quarantine manifest is not a private regular file".into());
    }
    let manifest: Value = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| format!("cannot read quarantine manifest: {error}"))?,
    )
    .map_err(|error| format!("quarantine manifest is invalid: {error}"))?;
    if manifest["schema_version"] != 1
        || manifest["operation_id"] != operation_id
        || manifest["common_dir"] != common.to_string_lossy().as_ref()
        || !matches!(
            manifest["phase"].as_str(),
            Some("completed_preserved" | "purging_preserved")
        )
    {
        return Err(
            "quarantine manifest is not a completed transaction for this repository".into(),
        );
    }
    let worktree = manifest["worktree"]
        .as_str()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "quarantine manifest has an invalid worktree path".to_string())?;
    if fs::symlink_metadata(&worktree).is_ok() {
        return Err(format!(
            "original worktree path still exists: {}",
            worktree.display()
        ));
    }
    let registrations = git::run(repo_root, ["worktree", "list", "--porcelain"])
        .map_err(|error| format!("cannot inspect worktree registrations: {}", error.msg))?;
    if registrations.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|path| Path::new(path) == worktree)
    }) {
        return Err("original worktree is still registered".into());
    }
    let head = manifest["head"]
        .as_str()
        .filter(|oid| git::is_oid(oid))
        .ok_or_else(|| "quarantine manifest has an invalid HEAD OID".to_string())?;
    let ancestry = git::output(repo_root, ["merge-base", "--is-ancestor", head, main_ref])
        .map_err(|error| error.msg)?;
    if !ancestry.status.success() {
        return Err(format!(
            "original worktree HEAD is not contained in {main_ref}"
        ));
    }
    let branch = manifest["branch_ref"].as_str();
    if let Some(branch) = branch {
        if !branch.starts_with("refs/heads/") {
            return Err("quarantine manifest has an invalid branch ref".into());
        }
        let current =
            git::output(repo_root, ["rev-parse", "--verify", branch]).map_err(|error| error.msg)?;
        if current.status.success() && String::from_utf8_lossy(&current.stdout).trim() != head {
            return Err(format!(
                "original worktree branch moved after removal: {branch}"
            ));
        }
    }

    let mut transaction = Transaction {
        operation: operation.clone(),
        manifest_path,
        manifest,
        moved: Vec::new(),
        database_link: None,
    };
    let mut allowed = BTreeSet::from(["manifest.json".to_string()]);
    let mut labels = Vec::new();
    let records = transaction.manifest["generated_caches"]
        .as_array()
        .ok_or_else(|| "quarantine manifest has no generated-cache array".to_string())?
        .clone();
    for (index, record) in records.iter().enumerate() {
        let relative = record["path"]
            .as_str()
            .ok_or_else(|| "generated artifact path is malformed".to_string())?;
        if !artifact_config::valid_quarantine_path(relative) {
            return Err(format!(
                "unrecognized generated-artifact path in manifest: {relative}"
            ));
        }
        let name = format!("generated-cache-{index:03}");
        let journal_name = format!("purge-{name}.jsonl");
        if record["purge_journal"]
            .as_str()
            .is_some_and(|value| value != journal_name)
        {
            return Err("generated artifact purge journal path is invalid".into());
        }
        if fs::symlink_metadata(operation.join(&journal_name)).is_ok() {
            allowed.insert(journal_name.clone());
        }
        if record["quarantine_path"] != operation.join(&name).to_string_lossy().as_ref() {
            return Err("generated artifact quarantine path is invalid".into());
        }
        if record["purge_state"] == "purged" {
            if fs::symlink_metadata(operation.join(&name)).is_ok() {
                return Err(format!("purged artifact remains in quarantine: {name}"));
            }
            continue;
        }
        let archive = operation.join(&name);
        let signature = parse_signature(&record["signature"])?;
        verify_artifact_progress(&archive, &signature, record, &operation.join(&journal_name))?;
        if fs::symlink_metadata(&archive).is_ok() {
            allowed.insert(name);
        }
        labels.push(relative.to_string());
        transaction.moved.push(MovedArtifact {
            original: worktree.join(relative),
            quarantine: archive,
            journal: operation.join(journal_name),
            signature,
            preserve: false,
            direnv: false,
        });
    }
    if transaction.manifest["direnv"].is_object() {
        let record = transaction.manifest["direnv"].clone();
        let archive = operation.join(".direnv");
        if record["purge_journal"]
            .as_str()
            .is_some_and(|value| value != "purge-direnv.jsonl")
        {
            return Err(".direnv purge journal path is invalid".into());
        }
        if fs::symlink_metadata(operation.join("purge-direnv.jsonl")).is_ok() {
            allowed.insert("purge-direnv.jsonl".into());
        }
        if record["quarantine_path"] != archive.to_string_lossy().as_ref() {
            return Err(".direnv quarantine path is invalid".into());
        }
        if record["purge_state"] != "purged" {
            let discardable = record["discardable"] == true;
            let signature = if discardable {
                parse_signature(&record["signature"])?
            } else if discard_unverified_direnv {
                let root_identity = fs::symlink_metadata(&archive)
                    .map_err(|error| format!("unverified .direnv is missing: {error}"))?;
                if !root_identity.is_dir() || root_identity.file_type().is_symlink() {
                    return Err("unverified .direnv purge requires a real directory; preserving its quarantine".into());
                }
                if record["identity"][0] != json!(root_identity.dev())
                    || record["identity"][1] != json!(root_identity.ino())
                {
                    return Err("unverified .direnv root identity changed".into());
                }
                let signature = sign_tree(&archive, archive.parent().unwrap(), true, false, true)?;
                if !dry_run {
                    transaction.manifest["direnv"]["signature"] = signature_json(&signature);
                    transaction.manifest["direnv"]["discardable"] = Value::Bool(true);
                    write_manifest(&transaction, "purging_preserved", false)?;
                }
                signature
            } else {
                return Err(
                    "purging an unverified .direnv requires --discard-unverified-direnv".into(),
                );
            };
            verify_artifact_progress(
                &archive,
                &signature,
                &transaction.manifest["direnv"],
                &operation.join("purge-direnv.jsonl"),
            )?;
            if fs::symlink_metadata(&archive).is_ok() {
                allowed.insert(".direnv".into());
            }
            labels.push(".direnv".into());
            transaction.moved.push(MovedArtifact {
                original: worktree.join(".direnv"),
                quarantine: archive,
                journal: operation.join("purge-direnv.jsonl"),
                signature,
                preserve: false,
                direnv: true,
            });
        }
    }
    let actual: BTreeSet<String> = fs::read_dir(&operation)
        .map_err(|error| format!("cannot inspect quarantine contents: {error}"))?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|error| format!("cannot read quarantine entry: {error}"))?;
    if actual != allowed {
        return Err("quarantine contains unrecognized or missing entries".into());
    }
    if dry_run {
        return Ok(labels);
    }
    for moved in transaction.moved.clone() {
        let record = transaction_record_mut(&mut transaction, &moved)?;
        record["purge_state"] = Value::String("purging".into());
        write_manifest(&transaction, "purging_preserved", false)?;
        purge_tree(&mut transaction, &moved)?;
        let record = transaction_record_mut(&mut transaction, &moved)?;
        record["purge_state"] = Value::String("purged".into());
        record.as_object_mut().unwrap().remove("purge_cursor");
        record.as_object_mut().unwrap().remove("purge_pending");
        record.as_object_mut().unwrap().remove("purge_warning");
        write_manifest(&transaction, "purging_preserved", false)?;
    }
    transaction.manifest["phase"] = Value::String("caches_purged".into());
    write_manifest(&transaction, "caches_purged", false)?;
    remove_transaction_dir(&transaction)?;
    Ok(labels)
}

fn private_dir(metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == unsafe { libc::getuid() }
        && metadata.mode() & 0o077 == 0
}

fn parse_signature(value: &Value) -> Result<Vec<TreeEntry>, String> {
    let entries = value
        .as_array()
        .ok_or_else(|| "artifact signature is malformed".to_string())?;
    let mut result = Vec::new();
    let mut paths = BTreeSet::new();
    for entry in entries {
        let values = entry
            .as_array()
            .ok_or_else(|| "artifact signature entry is malformed".to_string())?;
        if values.len() != 8 {
            return Err("artifact signature entry has an invalid field count".into());
        }
        let relative = values[0]
            .as_str()
            .ok_or_else(|| "signature path is invalid".to_string())?;
        if relative != "."
            && (relative.starts_with('/')
                || relative
                    .split('/')
                    .any(|part| matches!(part, "" | "." | "..")))
        {
            return Err("artifact signature has an unsafe path".into());
        }
        if !paths.insert(relative.to_string()) {
            return Err("artifact signature contains a duplicate path".into());
        }
        let kind = values[1]
            .as_str()
            .filter(|kind| matches!(*kind, "directory" | "file" | "symlink" | "special"))
            .ok_or_else(|| "artifact signature kind is invalid".to_string())?;
        let as_u64 = |index: usize| {
            values[index]
                .as_u64()
                .ok_or_else(|| "artifact signature number is invalid".to_string())
        };
        let mtime = values[6]
            .as_str()
            .and_then(|text| text.parse::<i128>().ok())
            .ok_or_else(|| "artifact signature timestamp is invalid".to_string())?;
        let mode = as_u64(4)?;
        if mode > u32::MAX as u64 {
            return Err("artifact signature mode is invalid".into());
        }
        result.push(TreeEntry {
            relative: relative.into(),
            kind: kind.into(),
            device: as_u64(2)?,
            inode: as_u64(3)?,
            mode: mode as u32,
            size: as_u64(5)?,
            mtime_ns: mtime,
            link_or_digest: values[7]
                .as_str()
                .ok_or_else(|| "artifact signature link is invalid".to_string())?
                .into(),
        });
    }
    if result.first().is_none_or(|entry| entry.relative != ".") {
        return Err("artifact signature has no root record".into());
    }
    Ok(result)
}

fn verify_purge_progress(
    root: &Path,
    signature: &[TreeEntry],
    record: &Value,
) -> Result<(), String> {
    let ordered = legacy_purge_order(signature);
    let cursor = record["purge_cursor"].as_u64().unwrap_or(0) as usize;
    let pending = record["purge_pending"].as_str();
    if cursor > ordered.len()
        || pending.is_some_and(|value| cursor >= ordered.len() || value != ordered[cursor].relative)
    {
        return Err("quarantine purge cursor is malformed".into());
    }
    if fs::symlink_metadata(root).is_err() {
        let root_index = ordered
            .iter()
            .position(|entry| entry.relative == ".")
            .ok_or_else(|| "artifact signature has no root record".to_string())?;
        if (pending == Some(".") && cursor == root_index)
            || (pending.is_none() && cursor == ordered.len())
        {
            return Ok(());
        }
        return Err("quarantine root disappeared before its signed purge step".into());
    }
    let mut expected_paths = BTreeSet::new();
    for (index, entry) in ordered.iter().enumerate() {
        let path = if entry.relative == "." {
            root.to_path_buf()
        } else {
            root.join(&entry.relative)
        };
        if index < cursor {
            if fs::symlink_metadata(&path).is_ok() {
                return Err(format!(
                    "purged artifact entry reappeared: {}",
                    path.display()
                ));
            }
        } else if index == cursor && pending == Some(entry.relative.as_str()) {
            if fs::symlink_metadata(&path).is_ok() {
                verify_one(&path, entry)?;
                expected_paths.insert(entry.relative.clone());
            }
        } else {
            verify_one(&path, entry)?;
            expected_paths.insert(entry.relative.clone());
        }
    }
    let mut observed = BTreeSet::new();
    collect_relative_paths(root, root, &mut observed)?;
    if observed != expected_paths {
        return Err(format!(
            "quarantine contents differ from signed purge state: {}",
            root.display()
        ));
    }
    Ok(())
}

fn verify_artifact_progress(
    root: &Path,
    signature: &[TreeEntry],
    record: &Value,
    journal: &Path,
) -> Result<(), String> {
    if record["purge_order"]
        .as_str()
        .is_some_and(|order| order != "legacy")
    {
        return Err("artifact purge order is invalid".into());
    }
    if record.get("purge_cursor").is_some() || record.get("purge_pending").is_some() {
        verify_purge_progress(root, signature, record)
    } else if fs::symlink_metadata(journal).is_ok() {
        let ordered = if record["purge_order"] == "legacy" {
            legacy_purge_order(signature)
        } else {
            purge_order(signature)
        };
        let states = read_purge_journal(journal, &ordered, false)?;
        verify_journal_progress(root, &ordered, &states)
    } else {
        verify_purge_progress(root, signature, record)
    }
}

fn purge_order(signature: &[TreeEntry]) -> Vec<TreeEntry> {
    let mut ordered = signature.to_vec();
    ordered.sort_by(|a, b| match (a.relative == ".", b.relative == ".") {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => b
            .relative
            .matches('/')
            .count()
            .cmp(&a.relative.matches('/').count())
            .then_with(|| b.relative.cmp(&a.relative)),
    });
    ordered
}

fn legacy_purge_order(signature: &[TreeEntry]) -> Vec<TreeEntry> {
    let mut ordered = signature.to_vec();
    ordered.sort_by(|a, b| {
        b.relative
            .matches('/')
            .count()
            .cmp(&a.relative.matches('/').count())
            .then_with(|| b.relative.cmp(&a.relative))
    });
    ordered
}

fn transaction_record_mut<'a>(
    transaction: &'a mut Transaction,
    moved: &MovedArtifact,
) -> Result<&'a mut Value, String> {
    if moved.direnv {
        let record = &mut transaction.manifest["direnv"];
        if record.is_object() {
            Ok(record)
        } else {
            Err("quarantine manifest has no .direnv record".into())
        }
    } else {
        let records = transaction.manifest["generated_caches"]
            .as_array_mut()
            .ok_or_else(|| "quarantine manifest has no generated-cache records".to_string())?;
        let index = moved
            .quarantine
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|name| name.strip_prefix("generated-cache-"))
            .and_then(|number| number.parse::<usize>().ok())
            .ok_or_else(|| "generated-cache quarantine name is invalid".to_string())?;
        records
            .get_mut(index)
            .ok_or_else(|| "generated-cache record is missing".to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PurgeJournalState {
    Pending,
    Deleted,
}

fn purge_tree(transaction: &mut Transaction, moved: &MovedArtifact) -> Result<(), String> {
    let record = transaction_record_mut(transaction, moved)?.clone();
    if record["purge_order"]
        .as_str()
        .is_some_and(|order| order != "legacy")
    {
        return Err("artifact purge order is invalid".into());
    }
    let legacy_order = record.get("purge_cursor").is_some()
        || record.get("purge_pending").is_some()
        || record["purge_order"] == "legacy";
    let ordered = if legacy_order {
        legacy_purge_order(&moved.signature)
    } else {
        purge_order(&moved.signature)
    };
    let journal_name = moved
        .journal
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "artifact purge journal name is invalid".to_string())?;
    if moved.journal.parent() != Some(transaction.operation.as_path())
        || record["purge_journal"]
            .as_str()
            .is_some_and(|value| value != journal_name)
    {
        return Err("artifact purge journal path is invalid".into());
    }

    let journal_meta = fs::symlink_metadata(&moved.journal);
    let legacy_progress =
        record.get("purge_cursor").is_some() || record.get("purge_pending").is_some();
    let mut states = if journal_meta.is_ok() && !legacy_progress {
        read_purge_journal(&moved.journal, &ordered, true)?
    } else {
        let mut states = Vec::new();
        if legacy_progress {
            verify_purge_progress(&moved.quarantine, &moved.signature, &record)?;
            let cursor = record["purge_cursor"].as_u64().unwrap_or(0) as usize;
            states.resize(cursor, Some(PurgeJournalState::Deleted));
            if let Some(pending) = record["purge_pending"].as_str() {
                if cursor >= ordered.len() || pending != ordered[cursor].relative {
                    return Err("quarantine purge cursor is malformed".into());
                }
                states.push(Some(PurgeJournalState::Pending));
            }
        }
        if journal_meta.is_ok() {
            let metadata = fs::symlink_metadata(&moved.journal)
                .map_err(|error| format!("cannot inspect artifact purge journal: {error}"))?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::getuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err("artifact purge journal is not a private regular file".into());
            }
            fs::remove_file(&moved.journal).map_err(|error| {
                format!("cannot restart artifact purge journal migration: {error}")
            })?;
        }
        let mut journal = create_purge_journal(&moved.journal)?;
        if !states.is_empty() {
            for (index, state) in states.iter().enumerate() {
                if *state == Some(PurgeJournalState::Deleted) {
                    append_purge_journal(&mut journal, index, &ordered[index], "deleted")?;
                } else {
                    append_purge_journal(&mut journal, index, &ordered[index], "pending")?;
                }
            }
            journal
                .sync_all()
                .map_err(|error| format!("cannot sync artifact purge journal: {error}"))?;
            sync_dir(&transaction.operation)?;
        } else {
            journal
                .sync_all()
                .map_err(|error| format!("cannot sync artifact purge journal: {error}"))?;
            sync_dir(&transaction.operation)?;
        }
        let record = transaction_record_mut(transaction, moved)?;
        record["purge_journal"] = Value::String(journal_name.into());
        if legacy_order {
            record["purge_order"] = Value::String("legacy".into());
        }
        record.as_object_mut().unwrap().remove("purge_cursor");
        record.as_object_mut().unwrap().remove("purge_pending");
        write_manifest(transaction, "purging_preserved", false)?;
        states
    };
    verify_journal_progress(&moved.quarantine, &ordered, &states)?;

    let open_paths = purge_open_paths(&moved.quarantine, &ordered, &states);
    let open_pids = lsof_open_paths(&open_paths)?;
    if !open_pids.is_empty() {
        return Err(format!(
            "generated artifact is open by process(es): {}",
            open_pids.join(", ")
        ));
    }

    let mut journal = open_purge_journal_append(&moved.journal)?;
    for start in (0..ordered.len()).step_by(PURGE_BATCH_ENTRIES) {
        let end = (start + PURGE_BATCH_ENTRIES).min(ordered.len());
        for (index, entry) in ordered.iter().enumerate().take(end).skip(start) {
            if index >= states.len() {
                append_purge_journal(&mut journal, index, entry, "pending")?;
                states.push(Some(PurgeJournalState::Pending));
            }
        }
        journal
            .sync_all()
            .map_err(|error| format!("cannot sync artifact purge journal: {error}"))?;

        let mut changed_dirs = BTreeSet::new();
        for (index, entry) in ordered.iter().enumerate().take(end).skip(start) {
            if states[index] == Some(PurgeJournalState::Deleted) {
                continue;
            }
            let path = if entry.relative == "." {
                moved.quarantine.clone()
            } else {
                moved.quarantine.join(&entry.relative)
            };
            changed_dirs.insert(
                path.parent()
                    .unwrap_or(&transaction.operation)
                    .to_path_buf(),
            );
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    verify_one(&path, entry)?;
                    match entry.kind.as_str() {
                        "directory" => fs::remove_dir(&path),
                        "file" | "symlink" => fs::remove_file(&path),
                        _ => {
                            return Err(format!(
                                "unsupported artifact type remains quarantined: {}",
                                path.display()
                            ))
                        }
                    }
                    .map_err(|error| format!("cannot purge {}: {error}", path.display()))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "cannot inspect pending purge entry {}: {error}",
                        path.display()
                    ))
                }
            }
        }
        for directory in changed_dirs {
            let mut ancestor = directory.as_path();
            while !fs::symlink_metadata(ancestor)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            {
                ancestor = ancestor.parent().ok_or_else(|| {
                    format!(
                        "cannot find a surviving parent directory for {}",
                        directory.display()
                    )
                })?;
            }
            sync_dir(ancestor)?;
        }
        for (index, entry) in ordered.iter().enumerate().take(end).skip(start) {
            if states[index] == Some(PurgeJournalState::Deleted) {
                continue;
            }
            append_purge_journal(&mut journal, index, entry, "deleted")?;
            states[index] = Some(PurgeJournalState::Deleted);
        }
        journal
            .sync_all()
            .map_err(|error| format!("cannot sync artifact purge journal: {error}"))?;
    }
    if states.len() != ordered.len()
        || states
            .iter()
            .any(|state| *state != Some(PurgeJournalState::Deleted))
    {
        return Err("generated artifact purge did not reach its signed end".into());
    }
    Ok(())
}

fn purge_open_paths(
    root: &Path,
    ordered: &[TreeEntry],
    states: &[Option<PurgeJournalState>],
) -> Vec<PathBuf> {
    ordered
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            states.get(*index).copied().flatten() != Some(PurgeJournalState::Deleted)
                && entry.kind != "symlink"
                && entry.kind != "special"
        })
        .map(|(_, entry)| {
            if entry.relative == "." {
                root.to_path_buf()
            } else {
                root.join(&entry.relative)
            }
        })
        .collect()
}

fn create_purge_journal(path: &Path) -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("cannot create artifact purge journal: {error}"))
}

fn open_purge_journal_append(path: &Path) -> Result<fs::File, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect artifact purge journal: {error}"))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("artifact purge journal is not a private regular file".into());
    }
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| format!("cannot open artifact purge journal: {error}"))
}

fn append_purge_journal(
    journal: &mut fs::File,
    index: usize,
    entry: &TreeEntry,
    state: &str,
) -> Result<(), String> {
    serde_json::to_writer(
        &mut *journal,
        &json!({"index": index, "path": entry.relative, "state": state}),
    )
    .map_err(|error| format!("cannot encode artifact purge journal: {error}"))?;
    journal
        .write_all(b"\n")
        .map_err(|error| format!("cannot append artifact purge journal: {error}"))
}

fn read_purge_journal(
    path: &Path,
    ordered: &[TreeEntry],
    repair_incomplete_tail: bool,
) -> Result<Vec<Option<PurgeJournalState>>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect artifact purge journal: {error}"))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("artifact purge journal is not a private regular file".into());
    }
    let mut states = Vec::new();
    let mut bytes =
        fs::read(path).map_err(|error| format!("cannot read artifact purge journal: {error}"))?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        let valid_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if repair_incomplete_tail {
            let journal = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|error| format!("cannot repair artifact purge journal tail: {error}"))?;
            journal
                .set_len(valid_len as u64)
                .and_then(|()| journal.sync_all())
                .map_err(|error| format!("cannot repair artifact purge journal tail: {error}"))?;
        }
        bytes.truncate(valid_len);
    }
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let value: Value = serde_json::from_slice(line)
            .map_err(|error| format!("artifact purge journal is malformed: {error}"))?;
        let index = value["index"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "artifact purge journal index is invalid".to_string())?;
        let path = value["path"]
            .as_str()
            .ok_or_else(|| "artifact purge journal path is invalid".to_string())?;
        let state = match value["state"].as_str() {
            Some("pending") => PurgeJournalState::Pending,
            Some("deleted") => PurgeJournalState::Deleted,
            _ => return Err("artifact purge journal state is invalid".into()),
        };
        if index >= ordered.len() || path != ordered[index].relative {
            return Err("artifact purge journal does not match the signed artifact".into());
        }
        match states.get(index).copied().flatten() {
            None if index == states.len() && state == PurgeJournalState::Pending => {
                states.push(Some(state));
            }
            None if index == states.len() && state == PurgeJournalState::Deleted => {
                states.push(Some(state));
            }
            Some(PurgeJournalState::Pending)
                if state == PurgeJournalState::Deleted
                    && states
                        .iter()
                        .position(|state| *state == Some(PurgeJournalState::Pending))
                        == Some(index) =>
            {
                states[index] = Some(state);
            }
            _ => return Err("artifact purge journal sequence is invalid".into()),
        }
    }
    Ok(states)
}

fn verify_journal_progress(
    root: &Path,
    ordered: &[TreeEntry],
    states: &[Option<PurgeJournalState>],
) -> Result<(), String> {
    if states.len() > ordered.len() {
        return Err("artifact purge journal exceeds the signed artifact".into());
    }
    let first_pending = states
        .iter()
        .position(|state| *state == Some(PurgeJournalState::Pending));
    if states.iter().any(Option::is_none)
        || first_pending.is_some_and(|index| {
            states[..index]
                .iter()
                .any(|state| *state != Some(PurgeJournalState::Deleted))
                || states[index..]
                    .iter()
                    .any(|state| *state != Some(PurgeJournalState::Pending))
        })
    {
        return Err("artifact purge journal sequence is incomplete".into());
    }
    let root_missing = path_is_missing(root)?;
    if root_missing
        && !(states.len() == ordered.len()
            && states
                .iter()
                .all(|state| *state == Some(PurgeJournalState::Deleted)))
        && !(states.last() == Some(&Some(PurgeJournalState::Pending))
            && ordered[states.len() - 1].relative == ".")
    {
        return Err("quarantine root disappeared before its signed purge step".into());
    }
    let mut expected_paths = BTreeSet::new();
    for (index, entry) in ordered.iter().enumerate() {
        let path = if entry.relative == "." {
            root.to_path_buf()
        } else {
            root.join(&entry.relative)
        };
        match states.get(index).copied().flatten() {
            Some(PurgeJournalState::Deleted) if path_is_missing(&path)? => {}
            Some(PurgeJournalState::Deleted) => {
                return Err(format!(
                    "purged artifact entry reappeared: {}",
                    path.display()
                ));
            }
            Some(PurgeJournalState::Pending) if path_is_missing(&path)? => {}
            Some(PurgeJournalState::Pending) | None => {
                verify_one(&path, entry)?;
                expected_paths.insert(entry.relative.clone());
            }
        }
    }
    if !root_missing {
        let mut observed = BTreeSet::new();
        collect_relative_paths(root, root, &mut observed)?;
        if observed != expected_paths {
            return Err(format!(
                "quarantine contents differ from signed purge journal: {}",
                root.display()
            ));
        }
    }
    Ok(())
}

fn path_is_missing(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!(
            "cannot inspect artifact path {}: {error}",
            path.display()
        )),
    }
}

fn verify_one(path: &Path, entry: &TreeEntry) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect signed artifact {}: {error}", path.display()))?;
    if metadata.dev() != entry.device
        || metadata.ino() != entry.inode
        || metadata.mode() != entry.mode
        || (if entry.kind == "directory" {
            0
        } else {
            metadata.size()
        }) != entry.size
        || (if entry.kind == "directory" {
            0
        } else {
            metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128
        }) != entry.mtime_ns
    {
        return Err(format!(
            "signed artifact changed before purge: {}",
            path.display()
        ));
    }
    if entry.kind == "symlink"
        && fs::read_link(path)
            .ok()
            .and_then(|link| link.to_str().map(str::to_string))
            .as_deref()
            != Some(entry.link_or_digest.as_str())
    {
        return Err(format!(
            "signed symlink changed before purge: {}",
            path.display()
        ));
    }
    if entry.kind == "file" && !entry.link_or_digest.is_empty() {
        let digest = hash_regular_file_nofollow(path, &metadata)?;
        if digest != entry.link_or_digest {
            return Err(format!(
                "signed file changed before purge: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn lsof_open_paths(paths: &[PathBuf]) -> Result<Vec<String>, String> {
    let Some(lsof) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("lsof"))
            .find(|path| path.is_file())
    }) else {
        return Err("lsof is required to verify generated-artifact handles".into());
    };
    let existing: Vec<_> = paths
        .iter()
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .collect();
    let mut pids = BTreeSet::new();
    let mut batch = Vec::new();
    let mut batch_bytes = 0;
    let mut batches = Vec::new();
    for path in existing {
        let size = path.as_os_str().as_bytes().len() + 1;
        if !batch.is_empty() && batch_bytes + size > LSOF_BATCH_BYTES {
            batches.push(std::mem::take(&mut batch));
            batch_bytes = 0;
        }
        batch_bytes += size;
        batch.push(path);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    for batch in batches {
        let mut command = std::process::Command::new(&lsof);
        command.arg("-Fpn");
        for path in batch {
            command.arg(path);
        }
        let output = command
            .output()
            .map_err(|error| format!("cannot run lsof: {error}"))?;
        if !matches!(output.status.code(), Some(0 | 1)) || !output.stderr.is_empty() {
            return Err(format!(
                "lsof could not verify generated-artifact handles: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let mut pid = None;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(value) = line.strip_prefix('p') {
                pid = Some(value.to_string());
            } else if line.starts_with('n') {
                if let Some(value) = &pid {
                    pids.insert(value.clone());
                }
            }
        }
    }
    Ok(pids.into_iter().collect())
}

pub fn open_path_users(paths: &[PathBuf]) -> Result<Vec<String>, String> {
    lsof_open_paths(paths)
}

fn signature_json(signature: &[TreeEntry]) -> Value {
    Value::Array(
        signature
            .iter()
            .map(|entry| {
                json!([
                    entry.relative,
                    entry.kind,
                    entry.device,
                    entry.inode,
                    entry.mode,
                    entry.size,
                    entry.mtime_ns.to_string(),
                    entry.link_or_digest,
                ])
            })
            .collect(),
    )
}

fn verify_signature(root: &Path, signature: &[TreeEntry], _worktree: &Path) -> Result<(), String> {
    let expected: BTreeSet<String> = signature
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    let root_entry = signature
        .iter()
        .find(|entry| entry.relative == ".")
        .ok_or_else(|| "artifact signature has no root entry".to_string())?;
    let root_meta = fs::symlink_metadata(root)
        .map_err(|error| format!("cannot inspect signed artifact {}: {error}", root.display()))?;
    if root_meta.dev() != root_entry.device || root_meta.ino() != root_entry.inode {
        return Err(format!("artifact identity changed: {}", root.display()));
    }
    if signature.len() == 1 {
        let kind = if root_meta.file_type().is_symlink() {
            "symlink"
        } else if root_meta.is_dir() {
            "directory"
        } else if root_meta.is_file() {
            "file"
        } else {
            "special"
        };
        if kind != root_entry.kind {
            return Err(format!("artifact root type changed: {}", root.display()));
        }
        if kind == "file" && !root_entry.link_or_digest.is_empty() {
            verify_one(root, root_entry)?;
        } else if kind == "symlink"
            && fs::read_link(root)
                .ok()
                .and_then(|target| target.to_str().map(str::to_string))
                .as_deref()
                != Some(root_entry.link_or_digest.as_str())
        {
            return Err(format!("artifact root symlink changed: {}", root.display()));
        }
        return Ok(());
    }
    if signature.len() > 1 {
        let mut observed = BTreeSet::new();
        collect_relative_paths(root, root, &mut observed)?;
        if observed != expected {
            return Err(format!("artifact tree changed: {}", root.display()));
        }
    }
    for entry in signature {
        let path = if entry.relative == "." {
            root.to_path_buf()
        } else {
            root.join(&entry.relative)
        };
        if entry.relative != "." {
            verify_parents(root, &path, signature)?;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect signed entry {}: {error}", path.display()))?;
        let kind = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "special"
        };
        let size = if kind == "directory" {
            0
        } else {
            metadata.size()
        };
        let mtime_ns = if kind == "directory" {
            0
        } else {
            metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128
        };
        if kind != entry.kind
            || metadata.dev() != entry.device
            || metadata.ino() != entry.inode
            || metadata.mode() != entry.mode
            || size != entry.size
            || mtime_ns != entry.mtime_ns
        {
            return Err(format!("signed artifact entry changed: {}", path.display()));
        }
        let actual = if kind == "symlink" {
            fs::read_link(&path)
                .map_err(|error| format!("cannot read signed symlink {}: {error}", path.display()))?
                .to_str()
                .ok_or_else(|| format!("signed symlink is not UTF-8: {}", path.display()))?
                .to_string()
        } else if !entry.link_or_digest.is_empty() {
            let bytes = read_regular_nofollow(&path, &metadata)?;
            format!("sha256:{}", hex(&Sha256::digest(bytes)))
        } else {
            String::new()
        };
        if actual != entry.link_or_digest {
            return Err(format!(
                "signed artifact content changed: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn collect_relative_paths(
    root: &Path,
    path: &Path,
    observed: &mut BTreeSet<String>,
) -> Result<(), String> {
    let relative = if path == root {
        ".".to_string()
    } else {
        path.strip_prefix(root)
            .map_err(|_| "artifact traversal escaped its root".to_string())?
            .to_str()
            .ok_or_else(|| "artifact contains a non-UTF-8 path".to_string())?
            .to_string()
    };
    observed.insert(relative);
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        for child in fs::read_dir(path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
        {
            collect_relative_paths(root, &child.map_err(|e| e.to_string())?.path(), observed)?;
        }
    }
    Ok(())
}

fn verify_parents(root: &Path, path: &Path, signature: &[TreeEntry]) -> Result<(), String> {
    let mut parent = path
        .parent()
        .ok_or_else(|| "artifact entry has no parent".to_string())?;
    while parent != root {
        let relative = parent
            .strip_prefix(root)
            .map_err(|_| "artifact parent escaped its root".to_string())?
            .to_str()
            .ok_or_else(|| "artifact parent path is not UTF-8".to_string())?;
        let expected = signature
            .iter()
            .find(|entry| entry.relative == relative)
            .ok_or_else(|| format!("artifact signature omits parent {relative}"))?;
        let metadata = fs::symlink_metadata(parent).map_err(|error| {
            format!(
                "cannot inspect artifact parent {}: {error}",
                parent.display()
            )
        })?;
        if expected.kind != "directory"
            || metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.dev() != expected.device
            || metadata.ino() != expected.inode
        {
            return Err(format!("artifact parent changed: {}", parent.display()));
        }
        parent = parent
            .parent()
            .ok_or_else(|| "artifact parent chain ended early".to_string())?;
    }
    Ok(())
}

fn verify_transaction_identity(
    transaction: &Transaction,
    worktree: &Path,
    admin: &Path,
    head: &str,
) -> Result<(), String> {
    let manifest = &transaction.manifest;
    if manifest["worktree"] != worktree.to_string_lossy().as_ref()
        || manifest["admin_dir"] != admin.to_string_lossy().as_ref()
        || manifest["head"] != head
    {
        return Err("quarantine transaction belongs to a different worktree state".into());
    }
    for (path, key) in [(worktree, "worktree_identity"), (admin, "admin_identity")] {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            format!(
                "cannot verify transaction identity {}: {error}",
                path.display()
            )
        })?;
        if manifest[key][0] != json!(metadata.dev()) || manifest[key][1] != json!(metadata.ino()) {
            return Err(format!("transaction identity changed: {}", path.display()));
        }
    }
    Ok(())
}

fn verify_database_link(link: &DatabaseLink) -> Result<(), String> {
    verify_database_target(link)?;
    let info = fs::symlink_metadata(&link.path)
        .map_err(|error| format!("database symlink changed: {error}"))?;
    if !info.file_type().is_symlink()
        || fs::read_link(&link.path).ok().as_ref() != Some(&link.link_text)
        || fs::canonicalize(&link.path).ok().as_ref() != Some(&link.target)
    {
        return Err("canonical state.db symlink changed".into());
    }
    Ok(())
}

fn restore_database_link(link: &DatabaseLink) -> Result<(), String> {
    if fs::symlink_metadata(&link.path).is_ok() {
        return Err(format!(
            "database restore path is occupied: {}",
            link.path.display()
        ));
    }
    verify_database_target(link)?;
    std::os::unix::fs::symlink(&link.link_text, &link.path).map_err(|error| {
        format!(
            "cannot restore database symlink {}: {error}",
            link.path.display()
        )
    })?;
    verify_database_link(link)
}

fn verify_database_target(link: &DatabaseLink) -> Result<(), String> {
    let metadata = fs::symlink_metadata(&link.target)
        .map_err(|error| format!("canonical database disappeared: {error}"))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.dev() != link.device
        || metadata.ino() != link.inode
    {
        return Err("canonical database target changed".into());
    }
    Ok(())
}

fn create_operation_dir(root: &Path) -> Result<PathBuf, String> {
    let mut bytes = [0u8; 16];
    for _ in 0..8 {
        fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|error| format!("cannot generate quarantine operation id: {error}"))?;
        let operation = root.join(hex(&bytes));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&operation) {
            Ok(()) => {
                sync_dir(root)?;
                return Ok(operation);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create quarantine transaction: {error}")),
        }
    }
    Err("could not allocate a unique quarantine operation id".into())
}

fn write_manifest(transaction: &Transaction, phase: &str, create: bool) -> Result<(), String> {
    let mut manifest = transaction.manifest.clone();
    manifest["phase"] = Value::String(phase.to_string());
    let temporary = transaction
        .operation
        .join(format!("manifest.{}.tmp", hex(&random_bytes()?)));
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    let result = (|| -> Result<(), String> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("cannot create transaction manifest: {error}"))?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.write_all(b"\n").map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        if create {
            rename_noreplace(&temporary, &transaction.manifest_path)?;
        } else {
            let info = fs::symlink_metadata(&transaction.manifest_path)
                .map_err(|error| format!("cannot inspect transaction manifest: {error}"))?;
            if !info.is_file() || info.file_type().is_symlink() {
                return Err("transaction manifest changed type".into());
            }
            fs::rename(&temporary, &transaction.manifest_path)
                .map_err(|error| format!("cannot update transaction manifest: {error}"))?;
        }
        sync_dir(&transaction.operation)
    })();
    if result.is_err() {
        if let Ok(metadata) = fs::symlink_metadata(&temporary) {
            if metadata.is_file() || metadata.file_type().is_symlink() {
                let _ = fs::remove_file(&temporary);
            }
        }
    }
    result
}

pub fn write_recovery_marker(transaction: &Transaction) -> Result<(), String> {
    write_manifest(transaction, "recovery_required", false)
}

fn random_bytes() -> Result<[u8; 8], String> {
    let mut bytes = [0u8; 8];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("cannot generate random token: {error}"))?;
    Ok(bytes)
}

fn remove_transaction_dir(transaction: &Transaction) -> Result<(), String> {
    let entries = fs::read_dir(&transaction.operation)
        .map_err(|error| format!("cannot inspect transaction directory: {error}"))?;
    let actual: BTreeSet<String> = entries
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    let mut journal_names = BTreeSet::new();
    if let Some(records) = transaction.manifest["generated_caches"].as_array() {
        for (index, record) in records.iter().enumerate() {
            let expected = format!("purge-generated-cache-{index:03}.jsonl");
            if record["purge_journal"]
                .as_str()
                .is_some_and(|name| name != expected)
            {
                return Err("generated artifact purge journal path is invalid".into());
            }
            if record["purge_journal"] == expected
                && (actual.contains(&expected)
                    || matches!(record["purge_state"].as_str(), Some("purging" | "purged")))
            {
                journal_names.insert(expected);
            }
        }
    }
    if transaction.manifest["direnv"].is_object() {
        if transaction.manifest["direnv"]["purge_journal"]
            .as_str()
            .is_some_and(|name| name != "purge-direnv.jsonl")
        {
            return Err(".direnv purge journal path is invalid".into());
        }
        if transaction.manifest["direnv"]["purge_journal"] == "purge-direnv.jsonl"
            && (actual.contains("purge-direnv.jsonl")
                || matches!(
                    transaction.manifest["direnv"]["purge_state"].as_str(),
                    Some("purging" | "purged")
                ))
        {
            journal_names.insert("purge-direnv.jsonl".into());
        }
    }
    let expected: BTreeSet<_> = std::iter::once("manifest.json".to_string())
        .chain(journal_names.iter().cloned())
        .collect();
    if actual != expected {
        return Err(format!(
            "unexpected contents in quarantine transaction: {}",
            transaction.operation.display()
        ));
    }
    let manifest = fs::symlink_metadata(&transaction.manifest_path)
        .map_err(|error| format!("cannot inspect transaction manifest: {error}"))?;
    if !manifest.is_file() || manifest.file_type().is_symlink() {
        return Err("transaction manifest changed type".into());
    }
    for name in journal_names {
        let path = transaction.operation.join(name);
        let journal = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect artifact purge journal: {error}"))?;
        if !journal.is_file()
            || journal.file_type().is_symlink()
            || journal.uid() != unsafe { libc::getuid() }
            || journal.mode() & 0o077 != 0
        {
            return Err("artifact purge journal is not a private regular file".into());
        }
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    fs::remove_file(&transaction.manifest_path).map_err(|error| error.to_string())?;
    fs::remove_dir(&transaction.operation).map_err(|error| error.to_string())?;
    sync_dir(transaction.operation.parent().unwrap())
}

fn sync_dir(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync directory {}: {error}", path.display()))
}

fn rename_noreplace(source: &Path, destination: &Path) -> Result<(), String> {
    let source_parent = source
        .parent()
        .ok_or_else(|| format!("source has no parent: {}", source.display()))?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| format!("destination has no parent: {}", destination.display()))?;
    let source_parent = open_directory_nofollow(source_parent)?;
    let destination_parent = open_directory_nofollow(destination_parent)?;
    let source_name = std::ffi::CString::new(
        source
            .file_name()
            .ok_or_else(|| format!("source has no file name: {}", source.display()))?
            .as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    let destination_name = std::ffi::CString::new(
        destination
            .file_name()
            .ok_or_else(|| format!("destination has no file name: {}", destination.display()))?
            .as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        renameatx_np(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            0x00000004,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = -1;
    if result != 0 {
        return Err(format!(
            "atomic no-overwrite rename {} -> {} failed: {}",
            source.display(),
            destination.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn open_directory_nofollow(path: &Path) -> Result<fs::File, String> {
    let resolved = fs::canonicalize(path).map_err(|error| {
        format!(
            "cannot resolve rename directory {}: {error}",
            path.display()
        )
    })?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&resolved)
        .map_err(|error| {
            format!(
                "cannot safely open rename directory {}: {error}",
                resolved.display()
            )
        })?;
    let opened = file.metadata().map_err(|error| {
        format!(
            "cannot inspect rename directory {}: {error}",
            resolved.display()
        )
    })?;
    let named = fs::symlink_metadata(&resolved).map_err(|error| {
        format!(
            "cannot recheck rename directory {}: {error}",
            resolved.display()
        )
    })?;
    if !opened.is_dir()
        || opened.dev() != named.dev()
        || opened.ino() != named.ino()
        || named.file_type().is_symlink()
    {
        return Err(format!(
            "rename directory changed while opening: {}",
            resolved.display()
        ));
    }
    Ok(file)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn renameatx_np(
        from_fd: libc::c_int,
        from: *const libc::c_char,
        to_fd: libc::c_int,
        to: *const libc::c_char,
        flags: libc::c_uint,
    ) -> libc::c_int;
}

pub fn inspect_plan(
    worktree: &Path,
    main_worktree: Option<&Path>,
    options: Options,
) -> Result<Plan, String> {
    let status = status(worktree)?;
    let caches = options.discard_caches || options.discard_all || options.preserve_generated;
    let build_outputs =
        options.discard_build_outputs || options.discard_all || options.preserve_generated;
    let include_config = options.discard_all || options.preserve_generated;
    let rules = artifact_config::load(main_worktree)?;
    let include_configured = options.discard_all || options.preserve_generated;
    let roots = generated_roots(
        &status,
        caches,
        build_outputs,
        include_config,
        include_configured,
        &rules,
    )?;
    let mut generated = Vec::new();
    for relative in &roots {
        let root_bytes = relative.as_os_str().as_bytes();
        let matching: Vec<_> = status
            .iter()
            .filter(|entry| under_root(&entry.path, root_bytes))
            .collect();
        if matching.is_empty()
            || matching
                .iter()
                .any(|entry| entry.code != *b"!!" && entry.code != *b"??")
        {
            return Err(format!(
                "generated artifact is not exclusively ignored or untracked: {}",
                worktree.join(relative).display()
            ));
        }
        let tracked = git::output(
            worktree,
            [
                OsStr::new("ls-files"),
                OsStr::new("--stage"),
                OsStr::new("--"),
                relative.as_os_str(),
            ],
        )
        .map_err(|error| format!("cannot check tracked files: {}", error.msg))?;
        if !tracked.status.success() || !tracked.stdout.is_empty() {
            return Err(format!(
                "generated artifact contains tracked files: {}",
                worktree.join(relative).display()
            ));
        }
        let path = worktree.join(relative);
        let allow_symlink =
            relative == Path::new("result") || relative == Path::new(PRECOMMIT_CONFIG);
        let signature = if rules.generated_artifacts.contains(relative) {
            sign_configured_artifact(&path, worktree)?
        } else {
            sign_tree(&path, worktree, allow_symlink, false, false)?
        };
        let is_build = matches!(relative.to_str(), Some("result" | "target" | "rust/target"));
        generated.push(SignedArtifact {
            path,
            signature,
            preserve: options.preserve_generated
                && !(options.discard_all || (options.discard_build_outputs && is_build)),
        });
    }

    let mut plan = Plan {
        generated,
        ..Plan::default()
    };
    let direnv_paths: Vec<_> = status
        .iter()
        .filter(|entry| is_direnv_path(&entry.path))
        .collect();
    if !direnv_paths.is_empty() {
        if direnv_paths
            .iter()
            .any(|entry| entry.code != *b"!!" && entry.code != *b"??")
        {
            return Err(".direnv contains tracked or modified files".into());
        }
        let tracked = git::output(worktree, ["ls-files", "--stage", "--", ".direnv"])
            .map_err(|error| format!("cannot inspect tracked .direnv files: {}", error.msg))?;
        if !tracked.status.success() || !tracked.stdout.is_empty() {
            return Err(".direnv contains tracked files".into());
        }
        let path = worktree.join(".direnv");
        plan.direnv_discardable = is_nix_direnv_cache(&path);
        let signature = if plan.direnv_discardable {
            sign_tree(&path, worktree, true, false, true)?
        } else {
            sign_root(&path)?
        };
        plan.discard_direnv = options.discard_direnv || options.discard_all;
        plan.direnv = Some(SignedArtifact {
            path,
            signature,
            preserve: !plan.discard_direnv || !plan.direnv_discardable,
        });
    }

    for entry in &status {
        if entry.code == *b"!!" && entry.path == b"state.db" {
            if let Some(link) = canonical_database_link(worktree, main_worktree) {
                plan.database_link = Some(link);
                continue;
            }
        }
        if plan.generated.iter().any(|artifact| {
            under_root(
                &entry.path,
                artifact
                    .path
                    .strip_prefix(worktree)
                    .unwrap()
                    .as_os_str()
                    .as_bytes(),
            )
        }) || is_direnv_path(&entry.path)
        {
            continue;
        }
        return Err(format!(
            "worktree has unrecognized changes ({:?}): {}",
            entry.code,
            String::from_utf8_lossy(&entry.path)
        ));
    }
    for artifact in plan.generated.iter().chain(plan.direnv.iter()) {
        let paths: Vec<_> = artifact
            .signature
            .iter()
            .filter(|entry| entry.kind != "symlink" && entry.kind != "special")
            .map(|entry| {
                if entry.relative == "." {
                    artifact.path.clone()
                } else {
                    artifact.path.join(&entry.relative)
                }
            })
            .collect();
        let open = lsof_open_paths(&paths)?;
        if !open.is_empty() {
            return Err(format!(
                "generated artifact is open by process(es): {}",
                open.join(", ")
            ));
        }
    }
    Ok(plan)
}

pub fn verify_plan_artifacts(
    plan: &Plan,
    worktree: &Path,
    main_worktree: Option<&Path>,
) -> Result<(), String> {
    for artifact in plan.generated.iter().chain(plan.direnv.iter()) {
        verify_signature(&artifact.path, &artifact.signature, worktree)?;
    }
    if let Some(link) = &plan.database_link {
        if canonical_database_link(worktree, main_worktree).as_ref() != Some(link) {
            return Err(format!(
                "canonical database link changed during merge: {}",
                link.path.display()
            ));
        }
    }
    Ok(())
}

pub fn check_pending_transaction(repo_name: &str, worktree: &Path) -> Result<(), String> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    if !state_home.is_absolute() {
        return Err("XDG_STATE_HOME must be absolute for worktree quarantine".into());
    }
    let root = state_home
        .join(repo_name)
        .join("worktrees/.removal-quarantine");
    let root_meta = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect quarantine root {}: {error}",
                root.display()
            ))
        }
    };
    if !private_dir(&root_meta) {
        return Err(format!(
            "quarantine root is not a private directory: {}",
            root.display()
        ));
    }
    for item in
        fs::read_dir(&root).map_err(|error| format!("cannot inspect quarantine root: {error}"))?
    {
        let operation = item.map_err(|error| error.to_string())?.path();
        let metadata = fs::symlink_metadata(&operation).map_err(|error| {
            format!(
                "cannot inspect quarantine entry {}: {error}",
                operation.display()
            )
        })?;
        if !private_dir(&metadata) {
            return Err(format!(
                "unexpected quarantine entry; inspect before removal: {}",
                operation.display()
            ));
        }
        let manifest_path = operation.join("manifest.json");
        let manifest_meta = fs::symlink_metadata(&manifest_path).map_err(|error| {
            format!(
                "quarantine manifest needs inspection: {} ({error})",
                manifest_path.display()
            )
        })?;
        if !manifest_meta.is_file()
            || manifest_meta.file_type().is_symlink()
            || manifest_meta.uid() != unsafe { libc::getuid() }
            || manifest_meta.mode() & 0o077 != 0
            || manifest_meta.size() > 64 * 1024 * 1024
        {
            return Err(format!(
                "quarantine manifest needs inspection: {}",
                manifest_path.display()
            ));
        }
        let manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| error.to_string())?)
                .map_err(|_| {
                format!(
                    "quarantine manifest needs inspection: {}",
                    manifest_path.display()
                )
            })?;
        if manifest["worktree"]
            .as_str()
            .is_some_and(|path| Path::new(path) == worktree)
            && manifest["phase"] != "completed_preserved"
        {
            return Err(format!(
                "an interrupted removal transaction needs inspection first: {}",
                manifest_path.display()
            ));
        }
    }
    Ok(())
}

fn under_root(path: &[u8], root: &[u8]) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with(b"/"))
}

fn is_direnv_path(path: &[u8]) -> bool {
    path == b".direnv" || path == b".direnv/" || path.starts_with(b".direnv/")
}

fn canonical_database_link(worktree: &Path, main_worktree: Option<&Path>) -> Option<DatabaseLink> {
    let main = main_worktree?;
    let canonical = main.join("state.db");
    let metadata = fs::symlink_metadata(&canonical).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let target = fs::canonicalize(&canonical).ok()?;
    if target != canonical || target == worktree || target.starts_with(worktree) {
        return None;
    }
    let link = worktree.join("state.db");
    if !fs::symlink_metadata(&link).ok()?.file_type().is_symlink()
        || fs::canonicalize(&link).ok()? != target
    {
        return None;
    }
    Some(DatabaseLink {
        path: link.clone(),
        link_text: fs::read_link(link).ok()?,
        target,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub fn generated_roots(
    status: &[StatusEntry],
    caches: bool,
    build_outputs: bool,
    generated_config: bool,
    include_configured: bool,
    rules: &artifact_config::Rules,
) -> Result<Vec<PathBuf>, String> {
    let mut candidates = BTreeSet::new();
    for entry in status {
        if entry.code != *b"!!" && entry.code != *b"??" {
            continue;
        }
        let path = std::str::from_utf8(&entry.path)
            .map_err(|_| "generated artifact path is not UTF-8".to_string())?;
        let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        if parts.is_empty() || parts[0] == ".direnv" {
            continue;
        }
        if caches {
            candidates.extend(
                rules
                    .cache_roots
                    .iter()
                    .filter(|root| configured_path_contains(path, root))
                    .cloned(),
            );
        }
        if caches {
            if let Some(index) = parts
                .iter()
                .position(|part| rules.cache_directory_names.contains(*part))
            {
                candidates.insert(parts[..=index].iter().collect::<PathBuf>());
            }
        }
        if build_outputs {
            candidates.extend(
                rules
                    .build_output_roots
                    .iter()
                    .filter(|root| configured_path_contains(path, root))
                    .cloned(),
            );
        }
        if generated_config {
            candidates.extend(
                rules
                    .generated_config_roots
                    .iter()
                    .filter(|root| configured_path_contains(path, root))
                    .cloned(),
            );
        }
        if include_configured {
            candidates.extend(
                rules
                    .generated_artifacts
                    .iter()
                    .filter(|root| configured_path_contains(path, root))
                    .cloned(),
            );
        }
    }

    let mut roots = Vec::new();
    for candidate in candidates {
        if !roots
            .iter()
            .any(|root: &PathBuf| candidate.starts_with(root))
        {
            roots.push(candidate);
        }
    }
    roots.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });
    Ok(roots)
}

fn configured_path_contains(path: &str, root: &Path) -> bool {
    let root = root.to_string_lossy();
    path == root.as_ref()
        || path
            .strip_prefix(root.as_ref())
            .is_some_and(|rest| rest.starts_with('/'))
}

pub fn sign_tree(
    root: &Path,
    worktree: &Path,
    allow_root_symlink: bool,
    allow_special_root: bool,
    allow_nested_symlinks: bool,
) -> Result<Vec<TreeEntry>, String> {
    sign_tree_with_store(
        root,
        worktree,
        allow_root_symlink,
        allow_special_root,
        allow_nested_symlinks,
        Path::new("/nix/store"),
    )
}

fn sign_configured_artifact(path: &Path, worktree: &Path) -> Result<Vec<TreeEntry>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect configured generated artifact {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "configured generated artifact must be a regular file: {}",
            path.display()
        ));
    }
    let mut signature = sign_tree(path, worktree, false, false, false)?;
    if signature.len() != 1 {
        return Err(format!(
            "configured generated artifact is not a single file: {}",
            path.display()
        ));
    }
    let entry = &mut signature[0];
    if entry.kind != "file"
        || entry.device != metadata.dev()
        || entry.inode != metadata.ino()
        || entry.mode != metadata.mode()
        || entry.size != metadata.size()
        || entry.mtime_ns
            != metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128
    {
        return Err(format!(
            "configured generated artifact changed while signing: {}",
            path.display()
        ));
    }
    entry.link_or_digest = hash_regular_file_nofollow(path, &metadata)?;
    Ok(signature)
}

fn sign_tree_with_store(
    root: &Path,
    worktree: &Path,
    allow_root_symlink: bool,
    allow_special_root: bool,
    allow_nested_symlinks: bool,
    store_root: &Path,
) -> Result<Vec<TreeEntry>, String> {
    let worktree_meta = fs::symlink_metadata(worktree).map_err(|error| {
        format!(
            "cannot inspect worktree root {}: {error}",
            worktree.display()
        )
    })?;
    if !worktree_meta.is_dir() || worktree_meta.file_type().is_symlink() {
        return Err(format!(
            "worktree root is not a real directory: {}",
            worktree.display()
        ));
    }
    let relative = root
        .strip_prefix(worktree)
        .map_err(|_| format!("artifact is outside the worktree: {}", root.display()))?;
    validate_relative(relative)?;
    let root_meta = fs::symlink_metadata(root).map_err(|error| {
        format!(
            "cannot inspect generated artifact {}: {error}",
            root.display()
        )
    })?;
    if root_meta.file_type().is_symlink() {
        if !allow_root_symlink || root_meta.dev() != worktree_meta.dev() {
            return Err(format!(
                "unrecognized generated artifact symlink: {}",
                root.display()
            ));
        }
    } else if !root_meta.is_dir() && !root_meta.is_file() && !allow_special_root {
        return Err(format!(
            "generated artifact has an unsupported type: {}",
            root.display()
        ));
    }
    if relative == Path::new(PRECOMMIT_CONFIG) && root_meta.file_type().is_symlink() {
        verify_precommit_symlink(root, store_root)?;
    }
    if root_meta.dev() != worktree_meta.dev() {
        return Err(format!(
            "generated artifact is on another filesystem: {}",
            root.display()
        ));
    }

    let mounts = if root_meta.is_dir() {
        let mounts = mount_points()?;
        if is_mount_point(root, &mounts)? {
            return Err(format!(
                "generated artifact is a mount point: {}",
                root.display()
            ));
        }
        mounts
    } else {
        BTreeSet::new()
    };

    let mut entries = Vec::new();
    sign_entry(
        root,
        root,
        &root_meta,
        &mut entries,
        allow_root_symlink || allow_nested_symlinks,
        allow_special_root,
        &mounts,
    )?;
    Ok(entries)
}

fn validate_relative(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("generated artifact path is not worktree-relative".into());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err("generated artifact path contains an unsafe component".into()),
        }
    }
    Ok(())
}

fn sign_root(path: &Path) -> Result<Vec<TreeEntry>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    let file_type = metadata.file_type();
    let (kind, link_or_digest) = if file_type.is_symlink() {
        let link = fs::read_link(path)
            .map_err(|error| format!("cannot read symlink {}: {error}", path.display()))?;
        (
            "symlink",
            link.to_str()
                .ok_or_else(|| format!("symlink target is not UTF-8: {}", path.display()))?
                .to_string(),
        )
    } else if metadata.is_dir() {
        ("directory", String::new())
    } else if metadata.is_file() {
        ("file", String::new())
    } else {
        ("special", String::new())
    };
    Ok(vec![TreeEntry {
        relative: ".".into(),
        kind: kind.into(),
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        size: metadata.size(),
        mtime_ns: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
        link_or_digest,
    }])
}

fn sign_entry(
    root: &Path,
    path: &Path,
    metadata: &fs::Metadata,
    entries: &mut Vec<TreeEntry>,
    allow_symlinks: bool,
    allow_special: bool,
    mounts: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    let relative = if path == root {
        ".".to_string()
    } else {
        path.strip_prefix(root)
            .map_err(|_| "artifact signature escaped its root".to_string())?
            .to_str()
            .ok_or_else(|| "generated artifact contains a non-UTF-8 path".to_string())?
            .to_string()
    };
    let file_type = metadata.file_type();
    let (kind, link_or_digest) = if file_type.is_symlink() {
        if path != root && !allow_symlinks {
            return Err(format!(
                "generated artifact contains a symlink: {}",
                path.display()
            ));
        }
        let link = fs::read_link(path)
            .map_err(|error| format!("cannot read symlink {}: {error}", path.display()))?;
        let text = link
            .to_str()
            .ok_or_else(|| {
                format!(
                    "generated artifact symlink is not UTF-8: {}",
                    path.display()
                )
            })?
            .to_string();
        ("symlink", text)
    } else if metadata.is_dir() {
        ("directory", String::new())
    } else if metadata.is_file() {
        if path
            .file_name()
            .is_some_and(|name| name == PRECOMMIT_CONFIG)
        {
            if metadata.len() > MAX_PRECOMMIT_BYTES {
                return Err(format!(
                    "generated pre-commit config exceeds size limit: {}",
                    path.display()
                ));
            }
            let bytes = read_regular_nofollow(path, metadata)?;
            if !bytes.starts_with(b"# DO NOT MODIFY\n# This file was generated by git-hooks.nix") {
                return Err(format!(
                    "{} is not a marked git-hooks.nix config",
                    path.display()
                ));
            }
            ("file", format!("sha256:{}", hex(&Sha256::digest(bytes))))
        } else {
            ("file", String::new())
        }
    } else if allow_special && path == root {
        ("special", String::new())
    } else {
        return Err(format!(
            "generated artifact contains a special file: {}",
            path.display()
        ));
    };
    entries.push(TreeEntry {
        relative,
        kind: kind.into(),
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        size: if kind == "directory" {
            0
        } else {
            metadata.size()
        },
        mtime_ns: if kind == "directory" {
            0
        } else {
            metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128
        },
        link_or_digest,
    });
    if metadata.is_dir() {
        if is_mount_point(path, mounts)? {
            return Err(format!(
                "generated artifact contains a mount point: {}",
                path.display()
            ));
        }
        let mut children: Vec<_> = fs::read_dir(path)
            .map_err(|error| {
                format!(
                    "cannot inspect artifact directory {}: {error}",
                    path.display()
                )
            })?
            .collect::<Result<_, _>>()
            .map_err(|error| {
                format!(
                    "cannot inspect artifact directory {}: {error}",
                    path.display()
                )
            })?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let child_path = child.path();
            let child_metadata = fs::symlink_metadata(&child_path)
                .map_err(|error| format!("cannot inspect {}: {error}", child_path.display()))?;
            if child.file_name() == ".git" {
                return Err(format!(
                    "generated artifact contains a nested Git repository: {}",
                    child_path.display()
                ));
            }
            if child_metadata.dev() != metadata.dev() {
                return Err(format!(
                    "generated artifact crosses a filesystem boundary: {}",
                    child_path.display()
                ));
            }
            sign_entry(
                root,
                &child_path,
                &child_metadata,
                entries,
                allow_symlinks,
                allow_special,
                mounts,
            )?;
        }
    }
    Ok(())
}

fn read_regular_nofollow(path: &Path, expected: &fs::Metadata) -> Result<Vec<u8>, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot safely open {}: {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect open file {}: {error}", path.display()))?;
    if !opened.is_file()
        || opened.dev() != expected.dev()
        || opened.ino() != expected.ino()
        || opened.mode() != expected.mode()
        || opened.size() != expected.size()
        || opened.mtime() != expected.mtime()
        || opened.mtime_nsec() != expected.mtime_nsec()
        || opened.size() > MAX_PRECOMMIT_BYTES
    {
        return Err(format!(
            "generated config changed while opening: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::with_capacity(opened.size() as usize);
    (&file)
        .take(MAX_PRECOMMIT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read generated config {}: {error}", path.display()))?;
    let after = file.metadata().map_err(|error| {
        format!(
            "cannot re-check generated config {}: {error}",
            path.display()
        )
    })?;
    if bytes.len() as u64 != opened.size()
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.size() != opened.size()
        || after.mtime() != opened.mtime()
        || after.mtime_nsec() != opened.mtime_nsec()
    {
        return Err(format!(
            "generated config changed while reading: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

fn hash_regular_file_nofollow(path: &Path, expected: &fs::Metadata) -> Result<String, String> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot safely open {}: {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect open file {}: {error}", path.display()))?;
    if !opened.is_file()
        || opened.dev() != expected.dev()
        || opened.ino() != expected.ino()
        || opened.mode() != expected.mode()
        || opened.size() != expected.size()
        || opened.mtime() != expected.mtime()
        || opened.mtime_nsec() != expected.mtime_nsec()
    {
        return Err(format!(
            "generated artifact changed while opening: {}",
            path.display()
        ));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| {
            format!("cannot hash generated artifact {}: {error}", path.display())
        })?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|error| {
        format!(
            "cannot re-check generated artifact {}: {error}",
            path.display()
        )
    })?;
    if after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.mode() != opened.mode()
        || after.size() != opened.size()
        || after.mtime() != opened.mtime()
        || after.mtime_nsec() != opened.mtime_nsec()
    {
        return Err(format!(
            "generated artifact changed while hashing: {}",
            path.display()
        ));
    }
    Ok(format!("sha256:{}", hex(&digest.finalize())))
}

fn mount_points() -> Result<BTreeSet<PathBuf>, String> {
    let mut points = BTreeSet::new();
    #[cfg(target_os = "linux")]
    {
        let contents = fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("cannot inspect mount table: {error}"))?;
        for line in contents.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            let Some(field) = fields.get(4) else {
                return Err("mount table entry is malformed".into());
            };
            points.insert(
                fs::canonicalize(decode_mount_field(field))
                    .map_err(|error| format!("cannot resolve mount point {field}: {error}"))?,
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut table: *mut libc::statfs = std::ptr::null_mut();
        let count = unsafe { libc::getmntinfo(&mut table, libc::MNT_NOWAIT) };
        if count <= 0 || table.is_null() {
            return Err("cannot inspect mount table with getmntinfo".into());
        }
        for index in 0..count as usize {
            let entry = unsafe { &*table.add(index) };
            let name = unsafe { std::ffi::CStr::from_ptr(entry.f_mntonname.as_ptr()) };
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes()));
            points.insert(fs::canonicalize(&path).map_err(|error| {
                format!("cannot resolve mount point {}: {error}", path.display())
            })?);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err("mount-point inspection is unsupported on this platform".into());
    Ok(points)
}

#[cfg(target_os = "linux")]
fn decode_mount_field(value: &str) -> PathBuf {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let escape = &bytes[index + 1..index + 4];
            let decoded_byte = match escape {
                b"040" => Some(b' '),
                b"011" => Some(b'\t'),
                b"012" => Some(b'\n'),
                b"134" => Some(b'\\'),
                _ => None,
            };
            if let Some(byte) = decoded_byte {
                decoded.push(byte);
                index += 4;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(decoded))
}

fn is_mount_point(path: &Path, mounts: &BTreeSet<PathBuf>) -> Result<bool, String> {
    let resolved = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve artifact path {}: {error}", path.display()))?;
    Ok(mounts.contains(&resolved))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    text
}

fn verify_precommit_symlink(path: &Path, store: &Path) -> Result<(), String> {
    let text = fs::read_link(path).map_err(|error| {
        format!(
            "cannot read generated config symlink {}: {error}",
            path.display()
        )
    })?;
    let target = if text.is_absolute() {
        text.clone()
    } else {
        return Err(format!(
            "generated config symlink is relative: {}",
            path.display()
        ));
    };
    if target.parent() != Some(store)
        || !target
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with("-pre-commit-config.json"))
        || fs::canonicalize(&target).ok().as_deref() != Some(target.as_path())
    {
        return Err(format!(
            "{} is not a Nix-store git-hooks config link",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(&target).map_err(|error| {
        format!(
            "cannot inspect generated config target {}: {error}",
            target.display()
        )
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_PRECOMMIT_BYTES
    {
        return Err(format!(
            "generated config target is not a supported regular file: {}",
            target.display()
        ));
    }
    let contents = fs::read(&target).map_err(|error| {
        format!(
            "cannot read generated config target {}: {error}",
            target.display()
        )
    })?;
    if !contents.starts_with(b"# DO NOT MODIFY\n# This file was generated by git-hooks.nix") {
        return Err(format!(
            "generated config target lacks the git-hooks.nix marker: {}",
            target.display()
        ));
    }
    Ok(())
}

fn is_nix_direnv_cache(root: &Path) -> bool {
    is_nix_direnv_cache_at(root, Path::new("/nix/store"))
}

fn is_nix_direnv_cache_at(root: &Path, store_root: &Path) -> bool {
    fn hash(value: &str) -> bool {
        (32..=64).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }
    fn nix_store_hash(value: &str) -> bool {
        value.len() == 32
            && value
                .bytes()
                .all(|byte| b"0123456789abcdfghijklmnpqrsvwxyz".contains(&byte))
    }
    fn store_link(path: &Path, expected: Option<&str>, store_root: &Path) -> bool {
        let Ok(link) = fs::read_link(path) else {
            return false;
        };
        if !link.is_absolute() || link.parent() != Some(store_root) {
            return false;
        }
        let Some(leaf) = link.file_name().and_then(OsStr::to_str) else {
            return false;
        };
        if let Some(expected) = expected {
            if leaf != expected {
                return false;
            }
            let Some(hash_prefix) = leaf.strip_suffix("-source") else {
                return false;
            };
            if !nix_store_hash(hash_prefix) {
                return false;
            }
        } else {
            let Some(hash_prefix) = leaf.strip_suffix("-nix-shell-env") else {
                return false;
            };
            if !nix_store_hash(hash_prefix) {
                return false;
            }
        }
        match fs::canonicalize(&link) {
            Ok(canonical) => {
                canonical == link
                    && fs::symlink_metadata(&link).is_ok_and(|metadata| {
                        !metadata.file_type().is_symlink()
                            && if expected.is_some() {
                                metadata.is_dir()
                            } else {
                                metadata.is_file() || metadata.is_dir()
                            }
                    })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::symlink_metadata(&link)
                    .is_err_and(|metadata| metadata.kind() == std::io::ErrorKind::NotFound)
            }
            Err(_) => false,
        }
    }
    let Ok(store_root) = fs::canonicalize(store_root) else {
        return false;
    };
    let Ok(root_meta) = fs::symlink_metadata(root) else {
        return false;
    };
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return false;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            return false;
        };
        names.insert(name);
    }
    if !names.contains("bin") || !names.contains("flake-inputs") {
        return false;
    }
    let bin = root.join("bin");
    let inputs = root.join("flake-inputs");
    let Ok(bin_meta) = fs::symlink_metadata(&bin) else {
        return false;
    };
    let Ok(inputs_meta) = fs::symlink_metadata(&inputs) else {
        return false;
    };
    if !bin_meta.is_dir()
        || bin_meta.file_type().is_symlink()
        || !inputs_meta.is_dir()
        || inputs_meta.file_type().is_symlink()
        || bin_meta.dev() != root_meta.dev()
        || inputs_meta.dev() != root_meta.dev()
    {
        return false;
    }
    let Ok(bin_entries) = fs::read_dir(&bin) else {
        return false;
    };
    let bin_entries: Vec<_> = bin_entries.collect();
    if bin_entries.len() != 1 {
        return false;
    }
    let Ok(reload) = &bin_entries[0] else {
        return false;
    };
    if reload.file_name() != "nix-direnv-reload" {
        return false;
    }
    let Ok(reload_meta) = fs::symlink_metadata(reload.path()) else {
        return false;
    };
    if !reload_meta.is_file() || reload_meta.mode() & 0o111 == 0 {
        return false;
    }
    let Ok(input_entries) = fs::read_dir(&inputs) else {
        return false;
    };
    for input in input_entries {
        let Ok(input) = input else { return false };
        let name = input.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix("-source") else {
            return false;
        };
        if !nix_store_hash(stem) || !store_link(&input.path(), Some(&name), &store_root) {
            return false;
        }
    }
    let mut profiles = BTreeSet::new();
    for name in &names {
        if let Some(stem) = name.strip_prefix("flake-profile-") {
            if let Some(profile) = stem.strip_suffix(".rc") {
                let rc = root.join(name);
                let Ok(metadata) = fs::symlink_metadata(&rc) else {
                    return false;
                };
                if !hash(profile) || !metadata.is_file() || metadata.file_type().is_symlink() {
                    return false;
                }
            } else if hash(stem) {
                let profile = root.join(name);
                if !store_link(&profile, None, &store_root) {
                    return false;
                }
                let Ok(link) = fs::read_link(&profile) else {
                    return false;
                };
                if !link
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|leaf| leaf.ends_with("-nix-shell-env"))
                {
                    return false;
                }
                profiles.insert(stem.to_string());
            } else {
                return false;
            }
        } else if !matches!(name.as_str(), "bin" | "flake-inputs") {
            return false;
        }
    }
    !profiles.is_empty()
        && profiles
            .iter()
            .all(|profile| root.join(format!("flake-profile-{profile}.rc")).is_file())
}

pub fn status(worktree: &Path) -> Result<Vec<StatusEntry>, String> {
    let output = git::output(worktree, git::STATUS_ARGS)
        .map_err(|error| format!("cannot run git status: {}", error.msg))?;
    if !output.status.success() {
        return Err(format!("git status failed: {}", git::stderr_of(&output)));
    }
    git::parse_status_entries(&output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;
    use std::time::{Duration, UNIX_EPOCH};

    fn temp(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("wtctl-artifact-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn selects_only_explicit_generated_roots_and_collapses_nested_roots() {
        let rules = artifact_config::load(None).unwrap();
        let status = [
            StatusEntry {
                code: *b"!!",
                path: b"target/debug/a".to_vec(),
            },
            StatusEntry {
                code: *b"??",
                path: b"src/__pycache__/x.pyc".to_vec(),
            },
            StatusEntry {
                code: *b"!!",
                path: b".direnv/flake-profile-x".to_vec(),
            },
            StatusEntry {
                code: *b"!!",
                path: b"notes.txt".to_vec(),
            },
        ];
        assert_eq!(
            generated_roots(&status, true, true, false, false, &rules).unwrap(),
            vec![PathBuf::from("target"), PathBuf::from("src/__pycache__")]
        );
        assert!(generated_roots(&status, false, false, false, false, &rules)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn selects_repository_configured_artifacts_only_when_cleanup_was_requested() {
        let mut rules = artifact_config::load(None).unwrap();
        rules
            .generated_artifacts
            .insert(PathBuf::from("output.csv"));
        let status = [
            StatusEntry {
                code: *b"!!",
                path: b"output.csv".to_vec(),
            },
            StatusEntry {
                code: *b"??",
                path: b"unlisted.csv".to_vec(),
            },
        ];
        assert!(generated_roots(&status, false, false, false, false, &rules)
            .unwrap()
            .is_empty());
        assert_eq!(
            generated_roots(&status, false, false, false, true, &rules).unwrap(),
            vec![PathBuf::from("output.csv")]
        );
    }

    #[test]
    fn inspect_plan_uses_committed_repository_artifact_configuration() {
        let root = temp("repo-configured-artifact");
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                arguments,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "wtctl test"]);
        git(&["config", "user.email", "wtctl-test@example.invalid"]);
        fs::write(root.join(".gitignore"), "output.csv\n").unwrap();
        fs::write(
            root.join(artifact_config::REPO_CONFIG),
            br#"{"schema_version":1,"generated_artifacts":["output.csv"]}"#,
        )
        .unwrap();
        git(&["add", ".gitignore", artifact_config::REPO_CONFIG]);
        git(&["commit", "-q", "-m", "artifact config"]);
        fs::write(root.join("output.csv"), "synthetic output\n").unwrap();

        let plan = inspect_plan(
            &root,
            Some(&root),
            Options {
                discard_all: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(plan.generated.len(), 1);
        assert_eq!(plan.generated[0].path, root.join("output.csv"));
        fs::remove_file(root.join("output.csv")).unwrap();
        fs::create_dir(root.join("output.csv")).unwrap();
        assert!(inspect_plan(
            &root,
            Some(&root),
            Options {
                discard_all: true,
                ..Options::default()
            },
        )
        .unwrap_err()
        .contains("must be a regular file"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn root_file_signature_detects_same_inode_same_size_rewrite_with_restored_mtime() {
        let worktree = temp("root-file-signature");
        let path = worktree.join("ranked_report.md");
        fs::write(&path, b"original").unwrap();
        let original = fs::metadata(&path).unwrap();
        let signature = sign_configured_artifact(&path, &worktree).unwrap();

        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"modified").unwrap();
        file.set_times(fs::FileTimes::new().set_modified(
            UNIX_EPOCH + Duration::new(original.mtime() as u64, original.mtime_nsec() as u32),
        ))
        .unwrap();

        let changed = fs::metadata(&path).unwrap();
        assert_eq!(changed.ino(), original.ino());
        assert_eq!(changed.size(), original.size());
        assert_eq!(changed.mtime(), original.mtime());
        assert_eq!(changed.mtime_nsec(), original.mtime_nsec());
        assert!(verify_signature(&path, &signature, &worktree).is_err());
        let _ = fs::remove_dir_all(worktree);
    }

    #[test]
    fn preserved_cache_with_special_ancestor_components_can_be_purged() {
        let fixture = temp("configured-cache-quarantine");
        let repo = fixture.join("repo");
        let state_home = fixture.join("state");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&state_home).unwrap();
        let task = repo.join("fake-task");
        let common = repo.join(".git");
        let admin = common.join("worktrees/fake-task");
        let artifact_paths = [
            task.join("experiments[1]/__pycache__"),
            task.join("experiments\\1/__pycache__"),
            task.join("subproject/.direnv/__pycache__"),
        ];
        let repo_name = format!("wtctl-test-{}", std::process::id());
        let state_namespace = state_home.join(&repo_name);

        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "wtctl test"]);
        git(
            &repo,
            &["config", "user.email", "wtctl-test@example.invalid"],
        );
        fs::write(repo.join("README.md"), "fixture\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "fixture"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        fs::create_dir_all(&admin).unwrap();
        for path in &artifact_paths {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("cache.pyc"), b"cache bytes").unwrap();
        }

        let plan = Plan {
            generated: artifact_paths
                .iter()
                .map(|path| SignedArtifact {
                    path: path.clone(),
                    signature: sign_tree(path, &task, false, false, false).unwrap(),
                    preserve: true,
                })
                .collect(),
            ..Plan::default()
        };
        let mut transaction = begin_transaction_in_state(
            &plan,
            BeginTransactionContext {
                repo_name: &repo_name,
                state_home: &state_home,
                common: &common,
                worktree: &task,
                admin: &admin,
                head: &head,
                branch: None,
            },
        )
        .unwrap()
        .unwrap();
        quarantine(&mut transaction, &plan, &task).unwrap();
        let report = finish_transaction(&mut transaction).unwrap();
        let operation = report.operation.unwrap();
        fs::remove_dir_all(&task).unwrap();

        let purged = purge_preserved_in_state(PurgeContext {
            state_home: &state_home,
            repo_root: &repo,
            repo_name: &repo_name,
            common: &common,
            main_ref: "refs/heads/main",
            operation_id: operation.file_name().unwrap().to_str().unwrap(),
            discard_unverified_direnv: false,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(
            purged,
            vec![
                "experiments[1]/__pycache__",
                "experiments\\1/__pycache__",
                "subproject/.direnv/__pycache__"
            ]
        );
        let _ = fs::remove_dir_all(state_namespace);
        let _ = fs::remove_dir_all(fixture);
    }

    #[test]
    fn preserved_custom_artifact_can_be_purged_after_repository_policy_changes() {
        let fixture = temp("custom-artifact-policy-purge");
        let repo = fixture.join("repo");
        let state_home = fixture.join("state");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&state_home).unwrap();
        let task = repo.join("fake-task");
        let common = repo.join(".git");
        let admin = common.join("worktrees/fake-task");
        let artifact_path = task.join("ranked_report.md");
        let repo_name = format!("wtctl-policy-test-{}", std::process::id());

        let git = |arguments: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                arguments,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "wtctl test"]);
        git(&["config", "user.email", "wtctl-test@example.invalid"]);
        fs::write(
            repo.join(artifact_config::REPO_CONFIG),
            br#"{"schema_version":1,"generated_artifacts":["ranked_report.md"]}"#,
        )
        .unwrap();
        git(&["add", artifact_config::REPO_CONFIG]);
        git(&["commit", "-q", "-m", "add artifact policy"]);
        let original_head = git(&["rev-parse", "HEAD"]);
        assert!(artifact_config::load(Some(&repo))
            .unwrap()
            .generated_artifacts
            .contains(Path::new("ranked_report.md")));

        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(&task).unwrap();
        fs::write(&artifact_path, "preserved report\n").unwrap();
        let plan = Plan {
            generated: vec![SignedArtifact {
                path: artifact_path.clone(),
                signature: sign_configured_artifact(&artifact_path, &task).unwrap(),
                preserve: true,
            }],
            ..Plan::default()
        };
        let mut transaction = begin_transaction_in_state(
            &plan,
            BeginTransactionContext {
                repo_name: &repo_name,
                state_home: &state_home,
                common: &common,
                worktree: &task,
                admin: &admin,
                head: &original_head,
                branch: None,
            },
        )
        .unwrap()
        .unwrap();
        quarantine(&mut transaction, &plan, &task).unwrap();
        let report = finish_transaction(&mut transaction).unwrap();
        let operation = report.operation.unwrap();
        fs::remove_dir_all(&task).unwrap();

        fs::write(
            repo.join(artifact_config::REPO_CONFIG),
            br#"{"schema_version":1,"generated_artifacts":[]}"#,
        )
        .unwrap();
        git(&["add", artifact_config::REPO_CONFIG]);
        git(&["commit", "-q", "-m", "remove artifact policy"]);
        assert!(artifact_config::load(Some(&repo))
            .unwrap()
            .generated_artifacts
            .is_empty());

        let moved = transaction.moved[0].clone();
        let ordered = purge_order(&moved.signature);
        let mut journal = create_purge_journal(&moved.journal).unwrap();
        append_purge_journal(&mut journal, 0, &ordered[0], "pending").unwrap();
        journal.sync_all().unwrap();
        drop(journal);
        transaction.manifest["generated_caches"][0]["purge_state"] =
            Value::String("purging".into());
        transaction.manifest["generated_caches"][0]["purge_journal"] =
            Value::String("purge-generated-cache-000.jsonl".into());
        write_manifest(&transaction, "purging_preserved", false).unwrap();
        fs::remove_file(&moved.quarantine).unwrap();

        let purged = purge_preserved_in_state(PurgeContext {
            state_home: &state_home,
            repo_root: &repo,
            repo_name: &repo_name,
            common: &common,
            main_ref: "refs/heads/main",
            operation_id: operation.file_name().unwrap().to_str().unwrap(),
            discard_unverified_direnv: false,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(purged, vec!["ranked_report.md"]);
        let _ = fs::remove_dir_all(fixture);
    }

    #[test]
    fn recognizes_only_the_supported_nix_direnv_use_flake_layout() {
        let root = temp("direnv");
        let cache = root.join(".direnv");
        let store = root.join("store");
        fs::create_dir_all(cache.join("bin")).unwrap();
        fs::create_dir_all(cache.join("flake-inputs")).unwrap();
        fs::create_dir_all(&store).unwrap();
        let input_hash = "a".repeat(32);
        let profile_hash = "b".repeat(32);
        let input = format!("{input_hash}-source");
        let profile = format!("{profile_hash}-nix-shell-env");
        fs::create_dir(store.join(&input)).unwrap();
        fs::create_dir(store.join(&profile)).unwrap();
        let store = fs::canonicalize(store).unwrap();
        fs::write(cache.join("bin/nix-direnv-reload"), "reload").unwrap();
        fs::set_permissions(
            cache.join("bin/nix-direnv-reload"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink(store.join(&input), cache.join("flake-inputs").join(&input)).unwrap();
        symlink(
            store.join(&profile),
            cache.join(format!("flake-profile-{profile_hash}")),
        )
        .unwrap();
        fs::write(
            cache.join(format!("flake-profile-{profile_hash}.rc")),
            "profile",
        )
        .unwrap();
        assert!(is_nix_direnv_cache_at(&cache, &store));

        fs::remove_dir(store.join(&input)).unwrap();
        fs::remove_dir(store.join(&profile)).unwrap();
        assert!(
            is_nix_direnv_cache_at(&cache, &store),
            "dangling links to collected Nix store outputs remain a stale cache"
        );

        let profile_link = cache.join(format!("flake-profile-{profile_hash}"));
        fs::remove_file(&profile_link).unwrap();
        symlink(
            root.join(format!("{profile_hash}-nix-shell-env")),
            &profile_link,
        )
        .unwrap();
        assert!(!is_nix_direnv_cache_at(&cache, &store));
        fs::remove_file(&profile_link).unwrap();
        symlink(store.join(&profile), &profile_link).unwrap();

        fs::write(store.join(&profile), "profile").unwrap();
        assert!(
            is_nix_direnv_cache_at(&cache, &store),
            "regular-file profile outputs are valid Nix-direnv targets"
        );
        fs::remove_file(store.join(&profile)).unwrap();
        assert!(
            is_nix_direnv_cache_at(&cache, &store),
            "dangling direct profile links remain stale caches"
        );

        fs::write(store.join(&input), "source").unwrap();
        assert!(
            !is_nix_direnv_cache_at(&cache, &store),
            "flake input source targets must remain directories"
        );

        fs::write(cache.join("unexpected"), "not part of nix-direnv").unwrap();
        assert!(!is_nix_direnv_cache_at(&cache, &store));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_precommit_config_requires_the_marker_and_a_nix_store_target() {
        let root = temp("precommit");
        let worktree = root.join("worktree");
        let store = root.join("store");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&store).unwrap();
        let store = fs::canonicalize(store).unwrap();
        let target = store.join("hash-pre-commit-config.json");
        fs::write(
            &target,
            b"# DO NOT MODIFY\n# This file was generated by git-hooks.nix\n{}\n",
        )
        .unwrap();
        let link = worktree.join(PRECOMMIT_CONFIG);
        symlink(&target, &link).unwrap();
        verify_precommit_symlink(&link, &store).unwrap();
        assert!(sign_tree_with_store(&link, &worktree, true, false, false, &store).is_ok());

        fs::write(&target, b"user edit\n").unwrap();
        assert!(verify_precommit_symlink(&link, &store).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signed_purge_resume_accepts_only_a_journaled_missing_root() {
        let root = temp("purge-resume");
        let artifact = root.join("artifact");
        fs::write(&artifact, b"selected output").unwrap();
        let signature = sign_tree(&artifact, &root, false, false, false).unwrap();
        fs::remove_file(&artifact).unwrap();
        assert!(verify_purge_progress(
            &artifact,
            &signature,
            &json!({"purge_cursor": 0, "purge_pending": "."}),
        )
        .is_ok());
        assert!(verify_purge_progress(&artifact, &signature, &json!({"purge_cursor": 0})).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn purge_journal_tracks_signed_entries_and_rejects_unexpected_content() {
        let root = temp("purge-journal");
        let artifact = root.join("artifact");
        fs::create_dir(&artifact).unwrap();
        fs::write(artifact.join("entry"), b"selected output").unwrap();
        let signature = sign_tree(&artifact, &root, false, false, false).unwrap();
        let ordered = purge_order(&signature);
        assert_eq!(purge_open_paths(&artifact, &ordered, &[]).len(), 2);
        let journal_path = root.join("purge.jsonl");
        let mut journal = create_purge_journal(&journal_path).unwrap();
        for (index, entry) in ordered.iter().enumerate() {
            append_purge_journal(&mut journal, index, entry, "pending").unwrap();
        }
        journal.sync_all().unwrap();
        let pending = read_purge_journal(&journal_path, &ordered, false).unwrap();
        verify_journal_progress(&artifact, &ordered, &pending).unwrap();
        fs::remove_file(artifact.join("entry")).unwrap();
        verify_journal_progress(
            &artifact,
            &ordered,
            &read_purge_journal(&journal_path, &ordered, false).unwrap(),
        )
        .unwrap();
        append_purge_journal(&mut journal, 0, &ordered[0], "deleted").unwrap();
        journal.sync_all().unwrap();
        let states = read_purge_journal(&journal_path, &ordered, false).unwrap();
        assert_eq!(
            states,
            [
                Some(PurgeJournalState::Deleted),
                Some(PurgeJournalState::Pending)
            ]
        );
        assert_eq!(
            purge_open_paths(&artifact, &ordered, &states).as_slice(),
            std::slice::from_ref(&artifact)
        );
        verify_journal_progress(&artifact, &ordered, &states).unwrap();
        fs::write(artifact.join("unexpected"), b"not signed").unwrap();
        assert!(verify_journal_progress(&artifact, &ordered, &states).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dry_run_journal_read_does_not_repair_an_incomplete_tail() {
        let root = temp("purge-journal-dry-run");
        let artifact = root.join("artifact");
        fs::create_dir(&artifact).unwrap();
        fs::write(artifact.join("entry"), b"selected output").unwrap();
        let signature = sign_tree(&artifact, &root, false, false, false).unwrap();
        let ordered = purge_order(&signature);
        let journal_path = root.join("purge.jsonl");
        let mut journal = create_purge_journal(&journal_path).unwrap();
        append_purge_journal(&mut journal, 0, &ordered[0], "pending").unwrap();
        journal.sync_all().unwrap();
        journal.write_all(b"{\"index\":1").unwrap();
        journal.sync_all().unwrap();
        drop(journal);
        let before = fs::read(&journal_path).unwrap();

        let states = read_purge_journal(&journal_path, &ordered, false).unwrap();
        assert_eq!(states, [Some(PurgeJournalState::Pending)]);
        assert_eq!(fs::read(&journal_path).unwrap(), before);

        let repaired = read_purge_journal(&journal_path, &ordered, true).unwrap();
        assert_eq!(repaired, [Some(PurgeJournalState::Pending)]);
        assert!(fs::read(&journal_path).unwrap().ends_with(b"\n"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_purge_order_keeps_root_last_for_every_child_name() {
        let entry = |relative: &str| TreeEntry {
            relative: relative.into(),
            kind: "file".into(),
            device: 0,
            inode: 0,
            mode: 0,
            size: 0,
            mtime_ns: 0,
            link_or_digest: String::new(),
        };
        let signature = [entry("."), entry("!entry")];
        let ordered = purge_order(&signature);
        assert_eq!(ordered.last().unwrap().relative, ".");
        assert_eq!(ordered.first().unwrap().relative, "!entry");
        assert_eq!(
            legacy_purge_order(&signature).first().unwrap().relative,
            "."
        );
    }
}
