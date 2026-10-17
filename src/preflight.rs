use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::git::{self, StatusEntry};

#[derive(Clone, Debug)]
struct TreePath {
    path: String,
    mode: String,
}

#[derive(Clone, Debug)]
struct ExtraPath {
    path: String,
    status: String,
}

pub fn check_merge_paths(
    worktree: &Path,
    current_tree: &str,
    final_tree: &str,
) -> Result<(), String> {
    let current = tree_paths(worktree, current_tree)?;
    let final_tree = tree_paths(worktree, final_tree)?;
    let status = git_status(worktree)?;
    let extras: Vec<ExtraPath> = status
        .into_iter()
        .filter(|entry| entry.code == *b"??" || entry.code == *b"!!")
        .map(|entry| {
            let path = String::from_utf8(entry.path).map_err(|_| {
                "an untracked path is not UTF-8; refusing a filesystem-unsafe merge".to_string()
            })?;
            let status = String::from_utf8_lossy(&entry.code).into_owned();
            Ok(ExtraPath { path, status })
        })
        .collect::<Result<_, String>>()?;
    check_path_sets(worktree, &current, &final_tree, &extras)
}

fn tree_paths(repo: &Path, treeish: &str) -> Result<Vec<TreePath>, String> {
    let output =
        git::output(repo, ["ls-tree", "-rz", "--full-tree", treeish]).map_err(|error| error.msg)?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect tree {treeish}: {}",
            git::stderr_of(&output)
        ));
    }
    let mut paths = Vec::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| format!("git ls-tree returned malformed output for {treeish}"))?;
        let fields: Vec<&[u8]> = record[..tab].split(|byte| *byte == b' ').collect();
        if fields.len() != 3 {
            return Err(format!(
                "git ls-tree returned malformed entry metadata for {treeish}"
            ));
        }
        let mode = std::str::from_utf8(fields[0])
            .map_err(|_| "git ls-tree returned a non-ASCII mode".to_string())?;
        let path = std::str::from_utf8(&record[tab + 1..]).map_err(|_| {
            "tracked Git path is not UTF-8; refusing a filesystem-unsafe merge".to_string()
        })?;
        paths.push(TreePath {
            path: path.to_string(),
            mode: mode.to_string(),
        });
    }
    Ok(paths)
}

fn git_status(worktree: &Path) -> Result<Vec<StatusEntry>, String> {
    let output = git::output(worktree, git::STATUS_ARGS).map_err(|error| error.msg)?;
    if !output.status.success() {
        return Err(format!("git status failed: {}", git::stderr_of(&output)));
    }
    git::parse_status_entries(&output.stdout)
}

fn path_key(path: &str) -> Result<Vec<String>, String> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(format!(
            "invalid Git path is not filesystem-representable: {path:?}"
        ));
    }
    if parts.iter().any(|part| part.ends_with(['.', ' '])) {
        return Err(format!("Git path has a trailing dot or space: {path:?}"));
    }
    let key: Vec<String> = parts
        .iter()
        .map(|part| part.nfd().case_fold().nfd().collect())
        .collect();
    if key.iter().any(|part| part == ".git") {
        return Err(format!("Git path aliases metadata: {path:?}"));
    }
    Ok(key)
}

fn prefixes(key: &[String]) -> impl Iterator<Item = &[String]> {
    (1..=key.len()).map(|length| &key[..length])
}

fn check_path_sets(
    worktree: &Path,
    current: &[TreePath],
    final_tree: &[TreePath],
    extras: &[ExtraPath],
) -> Result<(), String> {
    let mut final_by_key = BTreeMap::<Vec<String>, &TreePath>::new();
    let mut final_dirs = BTreeMap::<Vec<String>, Vec<String>>::new();
    for entry in final_tree {
        let key = path_key(&entry.path)?;
        if let Some(previous) = final_by_key.insert(key.clone(), entry) {
            if previous.path != entry.path {
                return Err(format!(
                    "final tree contains case/Unicode aliases: {:?} and {:?}",
                    previous.path, entry.path
                ));
            }
        }
        let parts: Vec<String> = entry.path.split('/').map(str::to_string).collect();
        for depth in 1..parts.len() {
            let prefix = key[..depth].to_vec();
            match final_dirs.entry(prefix) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(parts[..depth].to_vec());
                }
                std::collections::btree_map::Entry::Occupied(slot)
                    if slot.get() != &parts[..depth] =>
                {
                    return Err(format!(
                        "final tree contains directory aliases near {:?}",
                        entry.path
                    ));
                }
                _ => {}
            }
        }
    }
    for (key, entry) in &final_by_key {
        for prefix in prefixes(key).take(key.len().saturating_sub(1)) {
            if let Some(parent) = final_by_key.get(prefix) {
                return Err(format!(
                    "final tree contains a file/directory collision: {:?} and {:?}",
                    parent.path, entry.path
                ));
            }
        }
        if final_by_key
            .keys()
            .any(|other| other.len() > key.len() && other.starts_with(key))
        {
            return Err(format!(
                "final tree contains a file/directory collision at {:?}",
                entry.path
            ));
        }
    }

    let mut existing_by_key = BTreeMap::<Vec<String>, Vec<(String, String, bool)>>::new();
    let mut existing_dirs = BTreeMap::<Vec<String>, Option<Vec<String>>>::new();
    for entry in current {
        let key = path_key(&entry.path)?;
        existing_by_key.entry(key.clone()).or_default().push((
            entry.path.clone(),
            entry.mode.clone(),
            false,
        ));
        add_directory_spellings(&mut existing_dirs, &entry.path, &key)?;
    }
    for extra in extras {
        let path = extra.path.trim_end_matches('/');
        let key = path_key(path)?;
        existing_by_key.entry(key.clone()).or_default().push((
            path.to_string(),
            extra.status.clone(),
            true,
        ));
        add_directory_spellings(&mut existing_dirs, path, &key)?;
    }

    let mut extra_descendants = BTreeMap::<Vec<String>, Vec<String>>::new();
    for extra in extras {
        let path = extra.path.trim_end_matches('/');
        let key = path_key(path)?;
        for prefix in prefixes(&key) {
            extra_descendants
                .entry(prefix.to_vec())
                .or_default()
                .push(path.to_string());
        }
    }

    for (key, entry) in final_by_key {
        let path_parts: Vec<&str> = entry.path.split('/').collect();
        for depth in 1..key.len() {
            let prefix = key[..depth].to_vec();
            if let Some(old_spelling) = existing_dirs.get(&prefix) {
                if old_spelling
                    .as_ref()
                    .is_none_or(|old_spelling| old_spelling != &path_parts[..depth])
                {
                    return Err(format!(
                        "main/final directory spelling collision at {:?}",
                        entry.path
                    ));
                }
            }
            if let Some(nodes) = existing_by_key.get(&prefix) {
                if let Some((existing_path, _, _)) = nodes.first() {
                    return Err(format!(
                        "main file blocks final descendant: {existing_path:?} -> {:?}",
                        entry.path
                    ));
                }
            }
        }
        if let Some(nodes) = existing_by_key.get(&key) {
            for (existing_path, mode_or_status, extra) in nodes {
                if existing_path != &entry.path {
                    return Err(format!(
                        "main path aliases final path: {existing_path:?} and {:?}",
                        entry.path
                    ));
                }
                if *extra {
                    return Err(format!(
                        "final path would overwrite ignored/untracked data: {:?}",
                        entry.path
                    ));
                }
                if mode_or_status == "120000" && entry.mode != "120000" {
                    return Err(format!(
                        "final tree replaces a tracked symlink: {:?}",
                        entry.path
                    ));
                }
            }
        }
        if let Some(descendants) = extra_descendants.get(&key) {
            if descendants
                .iter()
                .any(|path| path_key(path).ok().as_ref() != Some(&key))
            {
                return Err(format!(
                    "final file would replace ignored/untracked descendants: {:?}",
                    entry.path
                ));
            }
        }
        let mut cursor = worktree.to_path_buf();
        for component in path_parts.iter().take(path_parts.len().saturating_sub(1)) {
            cursor.push(component);
            if fs::symlink_metadata(&cursor).is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(format!(
                    "final path traverses a worktree symlink: {:?} via {}",
                    entry.path,
                    cursor.display()
                ));
            }
        }
    }
    Ok(())
}

fn add_directory_spellings(
    directories: &mut BTreeMap<Vec<String>, Option<Vec<String>>>,
    path: &str,
    key: &[String],
) -> Result<(), String> {
    let parts: Vec<String> = path.split('/').map(str::to_string).collect();
    for depth in 1..parts.len() {
        match directories.entry(key[..depth].to_vec()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(parts[..depth].to_vec()));
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if entry
                    .get()
                    .as_ref()
                    .is_some_and(|old| old != &parts[..depth]) =>
            {
                entry.insert(None);
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_keys_apply_unicode_normalization_and_full_case_folding() {
        assert_eq!(
            path_key("Café/STRASSE").unwrap(),
            path_key("Cafe\u{301}/Straße").unwrap()
        );
        assert!(path_key("folder/.ＧＩＴ").is_ok());
        assert!(path_key("folder/.GIT").is_err());
        assert!(path_key("trailing.").is_err());
    }

    #[test]
    fn path_preflight_rejects_directory_aliases_and_ignored_overwrites() {
        let worktree = std::env::temp_dir().join(format!("wtctl-preflight-{}", std::process::id()));
        fs::create_dir_all(&worktree).unwrap();
        let current = [TreePath {
            path: "folder/task.txt".into(),
            mode: "100644".into(),
        }];
        let merged = [TreePath {
            path: "Folder/main.txt".into(),
            mode: "100644".into(),
        }];
        assert!(check_path_sets(&worktree, &current, &merged, &[])
            .unwrap_err()
            .contains("directory spelling collision"));

        let ignored = [ExtraPath {
            path: "ignored.txt".into(),
            status: "!!".into(),
        }];
        let final_file = [TreePath {
            path: "ignored.txt".into(),
            mode: "100644".into(),
        }];
        assert!(check_path_sets(&worktree, &[], &final_file, &ignored)
            .unwrap_err()
            .contains("ignored/untracked data"));
        let _ = fs::remove_dir_all(worktree);
    }
}
