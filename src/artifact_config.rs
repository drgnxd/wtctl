use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

const DEFAULTS: &str = include_str!("artifact_defaults.json");

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rules {
    pub cache_roots: BTreeSet<PathBuf>,
    pub cache_directory_names: BTreeSet<String>,
    pub build_output_roots: BTreeSet<PathBuf>,
    pub generated_config_roots: BTreeSet<PathBuf>,
}

pub fn load() -> Result<Rules, String> {
    let value: Value = serde_json::from_str(DEFAULTS)
        .map_err(|error| format!("built-in artifact policy is invalid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "built-in artifact policy must be a JSON object".to_string())?;
    let allowed = [
        "schema_version",
        "cache_roots",
        "cache_directory_names",
        "build_output_roots",
        "generated_config_roots",
    ];
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!(
            "built-in artifact policy contains unknown field {key:?}"
        ));
    }
    if object.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err("built-in artifact policy must use schema_version 1".into());
    }
    let generated_config_roots = path_set(object, "generated_config_roots")?;
    if generated_config_roots
        .iter()
        .any(|path| path != Path::new(".pre-commit-config.yaml"))
    {
        return Err(
            "built-in generated_config_roots may only select the marked pre-commit config".into(),
        );
    }
    Ok(Rules {
        cache_roots: path_set(object, "cache_roots")?,
        cache_directory_names: name_set(object, "cache_directory_names")?,
        build_output_roots: path_set(object, "build_output_roots")?,
        generated_config_roots,
    })
}

fn path_set(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<BTreeSet<PathBuf>, String> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("artifact policy field {field:?} must be an array"))?;
    values
        .iter()
        .map(|value| {
            let text = value
                .as_str()
                .ok_or_else(|| format!("artifact policy field {field:?} must contain strings"))?;
            validate_relative_path(text)
        })
        .collect()
}

fn name_set(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<BTreeSet<String>, String> {
    let values = object
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("artifact policy field {field:?} must be an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|name| {
                    !name.is_empty()
                        && !name.contains('/')
                        && !name.contains('\\')
                        && !matches!(*name, "." | ".." | ".git" | ".direnv")
                })
                .map(str::to_owned)
                .ok_or_else(|| format!("artifact policy field {field:?} must contain simple names"))
        })
        .collect()
}

fn validate_relative_path(text: &str) -> Result<PathBuf, String> {
    let path = Path::new(text);
    if text.is_empty()
        || text.contains('\\')
        || text.split('/').any(|part| matches!(part, "" | "." | ".."))
        || text
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']'))
        || path.is_absolute()
        || path.to_str() != Some(text)
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
        || path.components().enumerate().any(|(index, component)| {
            matches!(component, Component::Normal(name) if name == ".git" || (name == ".direnv" && index == 0))
        })
    {
        return Err(format!("unsafe or unsupported generated-artifact path {text:?}"));
    }
    Ok(path.to_path_buf())
}

fn validate_quarantine_path(text: &str) -> Result<PathBuf, String> {
    let path = Path::new(text);
    if text.is_empty()
        || text
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
        || path.is_absolute()
        || path.to_str() != Some(text)
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
        || path.components().enumerate().any(|(index, component)| {
            matches!(component, Component::Normal(name) if name == ".git" || (name == ".direnv" && index == 0))
        })
    {
        return Err(format!("unsafe or unsupported generated-artifact path {text:?}"));
    }
    Ok(path.to_path_buf())
}

pub fn valid_quarantine_path(path: &str) -> bool {
    validate_quarantine_path(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_only_builtin_artifact_roots() {
        let rules = load().unwrap();
        assert!(rules.cache_roots.contains(Path::new(".pytest_cache")));
        assert!(rules.build_output_roots.contains(Path::new("rust/target")));
        assert!(rules
            .generated_config_roots
            .contains(Path::new(".pre-commit-config.yaml")));
    }

    #[test]
    fn quarantine_paths_are_independent_of_repository_configuration() {
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
        ] {
            assert!(!valid_quarantine_path(path));
        }
    }
}
