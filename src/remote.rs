//! Opt-in network steps: `create --fetch` and `finish --push`. Neither runs
//! unless asked, and the push never forces.

use super::*;

pub fn remote_arg(a: &Args) -> R<String> {
    let name = a.get("--remote")?.unwrap_or("origin");
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !valid {
        return Err(Fail::new(
            2,
            format!("--remote {name:?} is not a remote name"),
        ));
    }
    Ok(name.to_string())
}

pub fn require_remote(repo: &Repo, name: &str) -> R<()> {
    let out = git::output(&repo.root, ["remote", "get-url", "--", name])?;
    if !out.status.success() {
        return Err(Fail::new(
            1,
            format!(
                "remote {name:?} is not configured in {}",
                repo.root.display()
            ),
        ));
    }
    Ok(())
}

/// Network steps must fail instead of waiting on a credential prompt.
fn network(repo: &Repo, args: &[&str]) -> R<std::process::Output> {
    git::command(&repo.root, args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| Fail::new(1, format!("cannot run git: {error}")))
}

pub fn fetch(repo: &Repo, name: &str) -> R<()> {
    require_remote(repo, name)?;
    deny_reference_hook(repo)?;
    let out = network(repo, &["fetch", "--quiet", "--", name])?;
    if !out.status.success() {
        return Err(Fail::new(
            1,
            format!("git fetch {name} failed: {}", git::stderr_of(&out)),
        ));
    }
    Ok(())
}

/// Validation that must pass before `finish` changes anything.
pub fn check_push(repo: &Repo, name: &str, allow_hooks: bool) -> R<()> {
    require_remote(repo, name)?;
    check_hooks(
        &active_hooks(
            &repo.root,
            &["pre-push", "reference-transaction"],
            None,
            None,
        )?,
        allow_hooks,
    )
}

pub fn push(repo: &Repo, name: &str, refname: &str) -> R<()> {
    let spec = format!("{refname}:{refname}");
    let out = network(repo, &["push", "--quiet", "--", name, &spec])?;
    if !out.status.success() {
        return Err(Fail::new(
            1,
            format!(
                "git push {name} {spec} failed (the integration itself succeeded): {}",
                git::stderr_of(&out)
            ),
        ));
    }
    Ok(())
}
