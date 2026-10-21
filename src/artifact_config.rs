use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::git;

pub const REPO_CONFIG: &str = ".wtctl.json";
const DEFAULTS: &str = include_str!("artifact_defaults.json");

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rules {
    pub cache_roots: BTreeSet<PathBuf>,
    pub cache_directory_names: BTreeSet<String>,
    pub build_output_roots: BTreeSet<PathBuf>,
    pub generated_config_roots: BTreeSet<PathBuf>,
    pub generated_artifacts: BTreeSet<PathBuf>,
    pub main_worktree_symlinks: BTreeSet<PathBuf>,
}

pub fn load(main_worktree: Option<&Path>) -> Result<Rules, String> {
    let mut rules = parse_defaults(DEFAULTS.as_bytes())?;
    let Some(main) = main_worktree else {
        return Ok(rules);
    };

    let tree_entry = git::output(
        main,
        [
            "ls-tree",
            "--format=%(objecttype) %(objectmode)",
            "HEAD",
            "--",
            REPO_CONFIG,
        ],
    )
    .map_err(|error| format!("cannot inspect {REPO_CONFIG}: {}", error.msg))?;
    if !tree_entry.status.success() {
        return Err(format!(
            "cannot inspect {REPO_CONFIG}: {}",
            git::stderr_of(&tree_entry)
        ));
    }
    let entry = String::from_utf8_lossy(&tree_entry.stdout);
    let entry = entry.trim();
    if entry.is_empty() {
        if fs::symlink_metadata(main.join(REPO_CONFIG)).is_ok() {
            return Err(format!(
                "{} exists in the main worktree but is not committed",
                main.join(REPO_CONFIG).display()
            ));
        }
        return Ok(rules);
    }
    if entry != "blob 100644" {
        return Err(format!(
            "{REPO_CONFIG} must be a regular tracked file with mode 100644"
        ));
    }

    // Read policy from main so a task worktree cannot authorize its own cleanup.
    let committed = git::output(main, ["show", &format!("HEAD:{REPO_CONFIG}")])
        .map_err(|error| format!("cannot read committed {REPO_CONFIG}: {}", error.msg))?;
    if !committed.status.success() {
        return Err(format!(
            "cannot read committed {REPO_CONFIG}: {}",
            git::stderr_of(&committed)
        ));
    }
    let configured = parse_repo_config(&committed.stdout)?;
    if configured.generated_artifacts.iter().any(|path| {
        rules.cache_roots.contains(path)
            || rules.build_output_roots.contains(path)
            || rules.generated_config_roots.contains(path)
            || path.file_name().is_some_and(|name| {
                rules
                    .cache_directory_names
                    .contains(&name.to_string_lossy().into_owned())
            })
    }) {
        return Err(format!(
            "{REPO_CONFIG} generated_artifacts cannot duplicate built-in artifact roots"
        ));
    }
    if configured.main_worktree_symlinks.iter().any(|path| {
        rules.cache_roots.contains(path)
            || rules.build_output_roots.contains(path)
            || rules.generated_config_roots.contains(path)
            || rules.generated_artifacts.contains(path)
            || configured.generated_artifacts.contains(path)
            || path.file_name().is_some_and(|name| {
                rules
                    .cache_directory_names
                    .contains(&name.to_string_lossy().into_owned())
            })
    }) {
        return Err(format!(
            "{REPO_CONFIG} main_worktree_symlinks cannot duplicate artifact roots"
        ));
    }
    rules
        .generated_artifacts
        .extend(configured.generated_artifacts);
    rules
        .main_worktree_symlinks
        .extend(configured.main_worktree_symlinks);
    Ok(rules)
}

fn parse_defaults(bytes: &[u8]) -> Result<Rules, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("built-in artifact policy is invalid JSON: {error}"))?;
    let object = object_with_keys(
        &value,
        &[
            "schema_version",
            "cache_roots",
            "cache_directory_names",
            "build_output_roots",
            "generated_config_roots",
        ],
        "built-in artifact policy",
    )?;
    if object.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err("built-in artifact policy must use schema_version 1".into());
    }
    Ok(Rules {
        cache_roots: path_set(object, "cache_roots")?,
        cache_directory_names: name_set(object, "cache_directory_names")?,
        build_output_roots: path_set(object, "build_output_roots")?,
        generated_config_roots: {
            let paths = path_set(object, "generated_config_roots")?;
            if paths
                .iter()
                .any(|path| path != Path::new(".pre-commit-config.yaml"))
            {
                return Err(
                    "built-in generated_config_roots may only select the marked pre-commit config"
                        .into(),
                );
            }
            paths
        },
        ..Rules::default()
    })
}

fn parse_repo_config(bytes: &[u8]) -> Result<Rules, String> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("{REPO_CONFIG} is invalid JSON: {error}"))?;
    let object = object_with_keys(
        &value,
        &[
            "schema_version",
            "generated_artifacts",
            "main_worktree_symlinks",
        ],
        REPO_CONFIG,
    )?;
    if object.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(format!("{REPO_CONFIG} must use schema_version 1"));
    }
    let paths = path_set(object, "generated_artifacts")?;
    if paths
        .iter()
        .any(|path| path.components().count() != 1 || path == Path::new(".pre-commit-config.yaml"))
    {
        return Err(format!(
            "{REPO_CONFIG} generated_artifacts entries must be repository-root regular-file paths"
        ));
    }
    let main_worktree_symlinks = if object.contains_key("main_worktree_symlinks") {
        path_set(object, "main_worktree_symlinks")?
    } else {
        BTreeSet::new()
    };
    if main_worktree_symlinks.iter().any(|path| {
        path.components().count() != 1
            || path == Path::new(".pre-commit-config.yaml")
            || paths.contains(path)
    }) {
        return Err(format!(
            "{REPO_CONFIG} main_worktree_symlinks entries must be distinct repository-root paths"
        ));
    }
    Ok(Rules {
        generated_artifacts: paths,
        main_worktree_symlinks,
        ..Rules::default()
    })
}

fn object_with_keys<'a>(
    value: &'a Value,
    allowed: &[&str],
    label: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be a JSON object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{label} contains unknown field {key:?}"));
    }
    Ok(object)
}

fn path_set(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<BTreeSet<PathBuf>, String> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("artifact policy field {field:?} must be an array"))?;
    let mut paths = BTreeSet::new();
    for value in values {
        let text = value
            .as_str()
            .ok_or_else(|| format!("artifact policy field {field:?} must contain strings"))?;
        let path = validate_relative_path(text)?;
        if !paths.insert(path) {
            return Err(format!(
                "artifact policy field {field:?} contains a duplicate path"
            ));
        }
    }
    Ok(paths)
}

fn name_set(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<BTreeSet<String>, String> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("artifact policy field {field:?} must be an array"))?;
    let mut names = BTreeSet::new();
    for value in values {
        let name = value
            .as_str()
            .filter(|name| {
                !name.is_empty()
                    && !name.contains('/')
                    && !name.contains('\\')
                    && !matches!(*name, "." | ".." | ".git" | ".direnv")
            })
            .ok_or_else(|| format!("artifact policy field {field:?} must contain simple names"))?;
        if !names.insert(name.to_owned()) {
            return Err(format!(
                "artifact policy field {field:?} contains a duplicate name"
            ));
        }
    }
    Ok(names)
}

fn validate_relative_path(text: &str) -> Result<PathBuf, String> {
    validate_path(text, true, true, true)
}

fn validate_quarantine_path(text: &str) -> Result<PathBuf, String> {
    validate_path(text, false, false, false)
}

fn validate_path(
    text: &str,
    reject_globs: bool,
    reject_backslash: bool,
    reject_nested_direnv: bool,
) -> Result<PathBuf, String> {
    let path = Path::new(text);
    if text.is_empty()
        || (reject_backslash && text.contains('\\'))
        || text
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
        || (reject_globs && text
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']')))
        || path.is_absolute()
        || path.to_str() != Some(text)
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
        || path.components().enumerate().any(|(index, component)| {
            matches!(component, Component::Normal(name) if name == ".git" || (name == ".direnv" && (reject_nested_direnv || index == 0)))
        })
        || path == Path::new(REPO_CONFIG)
    {
        return Err(format!("unsafe or unsupported generated-artifact path {text:?}"));
    }
    Ok(path.to_path_buf())
}

// A quarantine transaction owns the artifact signature and only purges inside its private directory.
pub fn valid_quarantine_path(path: &str) -> bool {
    validate_quarantine_path(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    fn fixture_repo() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wtctl-artifact-config-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let output = Command::new("git")
            .args(["-C", root.to_str().unwrap(), "init", "-q"])
            .output()
            .unwrap();
        assert!(output.status.success());
        for (key, value) in [
            ("user.name", "wtctl test"),
            ("user.email", "wtctl-test@example.invalid"),
        ] {
            let output = Command::new("git")
                .args(["-C", root.to_str().unwrap(), "config", key, value])
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        git(&root, &["commit", "--allow-empty", "-q", "-m", "initial"]);
        root
    }

    fn git(root: &Path, arguments: &[&str]) {
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
    }

    #[test]
    fn repository_policy_adds_safe_exact_paths_to_defaults() {
        let mut rules = parse_defaults(DEFAULTS.as_bytes()).unwrap();
        let configured = parse_repo_config(
            br#"{"schema_version":1,"generated_artifacts":["output.csv","monthly_report.md"],"main_worktree_symlinks":["state.db"]}"#,
        )
        .unwrap();
        rules
            .generated_artifacts
            .extend(configured.generated_artifacts.iter().cloned());
        rules
            .main_worktree_symlinks
            .extend(configured.main_worktree_symlinks.iter().cloned());
        assert!(rules.generated_artifacts.contains(Path::new("output.csv")));
        assert!(rules
            .generated_artifacts
            .contains(Path::new("monthly_report.md")));
        assert!(rules.main_worktree_symlinks.contains(Path::new("state.db")));
        assert!(rules.cache_roots.contains(Path::new(".pytest_cache")));
        assert!(rules.build_output_roots.contains(Path::new("rust/target")));
    }

    #[test]
    fn repository_policy_rejects_traversal_globs_and_reserved_paths() {
        for path in [
            "../outside.csv",
            "/tmp/output.csv",
            "reports/*.csv",
            "reports/monthly.md",
            ".git/config",
            ".direnv/cache",
            ".wtctl.json",
        ] {
            let bytes = format!(r#"{{"schema_version":1,"generated_artifacts":[{path:?}]}}"#);
            assert!(
                parse_repo_config(bytes.as_bytes()).is_err(),
                "accepted {path:?}"
            );
        }
    }

    #[test]
    fn main_worktree_symlinks_require_distinct_root_paths() {
        for path in ["../outside.db", "/tmp/state.db", "nested/state.db"] {
            let bytes = format!(
                r#"{{"schema_version":1,"generated_artifacts":[],"main_worktree_symlinks":[{path:?}]}}"#
            );
            assert!(
                parse_repo_config(bytes.as_bytes()).is_err(),
                "accepted {path:?}"
            );
        }
        assert!(parse_repo_config(
            br#"{"schema_version":1,"generated_artifacts":["state.db"],"main_worktree_symlinks":["state.db"]}"#
        )
        .is_err());
    }

    #[test]
    fn artifact_policy_rejects_unknown_schema_fields_and_duplicate_paths() {
        assert!(parse_repo_config(
            br#"{"schema_version":1,"generated_artifacts":[],"other":true}"#
        )
        .is_err());
        assert!(parse_repo_config(
            br#"{"schema_version":1,"generated_artifacts":["output.csv","output.csv"]}"#
        )
        .is_err());
    }

    #[test]
    fn quarantine_paths_are_independent_of_later_repository_config_changes() {
        for path in [
            "output.csv",
            "reports/monthly.csv",
            "experiments[1]/__pycache__",
            "subproject/.direnv/__pycache__",
        ] {
            assert!(valid_quarantine_path(path));
        }
        for path in [
            "../outside.csv",
            "/tmp/output.csv",
            "experiments//1/__pycache__",
            ".git/config",
            ".direnv/cache",
            ".wtctl.json",
        ] {
            assert!(!valid_quarantine_path(path));
        }
    }

    #[test]
    fn load_reads_only_a_committed_repository_policy() {
        let root = fixture_repo();
        let config = root.join(REPO_CONFIG);
        fs::write(
            &config,
            br#"{"schema_version":1,"generated_artifacts":["output.csv"],"main_worktree_symlinks":["state.db"]}"#,
        )
        .unwrap();
        git(&root, &["add", REPO_CONFIG]);
        git(&root, &["commit", "-q", "-m", "add artifact policy"]);

        let rules = load(Some(&root)).unwrap();
        assert!(rules.generated_artifacts.contains(Path::new("output.csv")));
        assert!(rules.main_worktree_symlinks.contains(Path::new("state.db")));

        fs::write(
            &config,
            br#"{"schema_version":1,"generated_artifacts":["different.csv"],"main_worktree_symlinks":["different.db"]}"#,
        )
        .unwrap();
        let rules = load(Some(&root)).unwrap();
        assert!(rules.generated_artifacts.contains(Path::new("output.csv")));
        assert!(!rules
            .generated_artifacts
            .contains(Path::new("different.csv")));
        assert!(!rules
            .main_worktree_symlinks
            .contains(Path::new("different.db")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn untracked_repository_policy_fails_closed() {
        let root = fixture_repo();
        fs::write(
            root.join(REPO_CONFIG),
            br#"{"schema_version":1,"generated_artifacts":["output.csv"]}"#,
        )
        .unwrap();
        assert!(load(Some(&root))
            .unwrap_err()
            .contains("exists in the main worktree but is not committed"));
        let _ = fs::remove_dir_all(root);
    }
}
