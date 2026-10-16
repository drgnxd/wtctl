use std::ffi::OsStr;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use crate::Fail;

// Inherited repository overrides would redirect every call to another repository.
const INHERITED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
];

pub fn command<I, S>(cwd: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(args);
    for name in INHERITED {
        cmd.env_remove(name);
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    if crate::lock::SIGNALS_BLOCKED.load(std::sync::atomic::Ordering::SeqCst) {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure only calls the async-signal-safe sigprocmask.
        unsafe { cmd.pre_exec(crate::lock::unblock_in_child) };
    }
    cmd
}

pub fn output<I, S>(cwd: &Path, args: I) -> Result<Output, Fail>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    command(cwd, args)
        .output()
        .map_err(|e| Fail::new(1, format!("cannot run git: {e}")))
}

pub fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

pub fn run<I, S>(cwd: &Path, args: I) -> Result<String, Fail>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = output(cwd, args)?;
    if !out.status.success() {
        return Err(Fail::new(1, format!("git failed: {}", stderr_of(&out))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Spawns with stdout captured and stderr discarded, so each child costs one pipe.
pub fn spawn(cwd: &Path, args: &[&str], stdin: bool) -> Result<Child, Fail> {
    command(cwd, args)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Fail::new(1, format!("cannot run git: {e}")))
}

/// Spawns for the exit status only; no pipes are created.
pub fn spawn_status(cwd: &Path, args: &[&str]) -> Result<Child, Fail> {
    command(cwd, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Fail::new(1, format!("cannot run git: {e}")))
}

pub const STATUS_ARGS: &[&str] = &[
    "status",
    "--porcelain=v1",
    "-z",
    "--untracked-files=all",
    "--ignored=matching",
];

// Directories the legacy remover also treats as disposable caches.
const DISPOSABLE: &[&str] = &["__pycache__", ".pytest_cache", ".ruff_cache"];

#[derive(Default, Debug, Clone, Copy)]
pub struct Cleanliness {
    pub tracked: usize,
    pub untracked: usize,
    pub ignored: usize,
}

impl Cleanliness {
    pub fn is_clean(&self) -> bool {
        self.tracked == 0 && self.untracked == 0 && self.ignored == 0
    }
}

pub fn parse_status(raw: &[u8]) -> Cleanliness {
    let mut c = Cleanliness::default();
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        let (Some(xy), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        match xy {
            b"??" => c.untracked += 1,
            b"!!" => {
                let path = String::from_utf8_lossy(path);
                if !path.split('/').any(|part| DISPOSABLE.contains(&part)) {
                    c.ignored += 1;
                }
            }
            _ => {
                c.tracked += 1;
                if matches!(xy.first(), Some(b'R' | b'C')) {
                    fields.next();
                }
            }
        }
    }
    c
}

/// Counts index entries hidden from `git status` by assume-unchanged or skip-worktree.
pub fn count_hidden(ls_files_v: &[u8]) -> usize {
    ls_files_v
        .split(|b| *b == 0)
        .filter_map(|e| e.first())
        .filter(|tag| tag.is_ascii_lowercase() || **tag == b'S')
        .count()
}

pub fn is_oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn executable_hooks(hooks_dir: &Path, names: &[&str]) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    names
        .iter()
        .filter(|name| {
            std::fs::metadata(hooks_dir.join(name))
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|s| s.to_string())
        .collect()
}
