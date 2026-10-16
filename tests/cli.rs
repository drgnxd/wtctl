//! End-to-end tests against throwaway repositories with real Git.
//!
//! The test binary itself plays the coding agent: `WTCTL_AGENT_NAMES` (only
//! honored with the `test-agent-names` feature) names it, so every wtctl
//! child finds this process as its owning ancestor.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Env {
    tmp: PathBuf,
    repo: PathBuf,
    agent: String,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

fn agent_name() -> String {
    std::env::current_exe()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(args);
    for (k, v) in base_env() {
        c.env(k, v);
    }
    c
}

fn base_env() -> Vec<(&'static str, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_AUTHOR_NAME", "t".into()),
        ("GIT_AUTHOR_EMAIL", "t@example.invalid".into()),
        ("GIT_COMMITTER_NAME", "t".into()),
        ("GIT_COMMITTER_EMAIL", "t@example.invalid".into()),
    ]
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, name: &str, body: &str) -> String {
    fs::write(dir.join(name), body).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-q", "-m", name]);
    git(dir, &["rev-parse", "HEAD"])
}

impl Env {
    fn new() -> Env {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!("wtctl-it-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let repo = tmp.join("proj");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(tmp.join("home")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "*.db\n__pycache__/\n").unwrap();
        git(&repo, &["add", ".gitignore"]);
        commit_file(&repo, "f.txt", "base\n");
        let repo = fs::canonicalize(repo).unwrap();
        Env {
            tmp: fs::canonicalize(&tmp).unwrap(),
            repo,
            agent: agent_name(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wtctl"));
        c.args(args)
            .env("HOME", self.tmp.join("home"))
            .env("XDG_STATE_HOME", self.tmp.join("state"))
            .env("WTCTL_AGENT_NAMES", &self.agent)
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("OPENCODE_SESSION_ID")
            .current_dir(&self.tmp);
        for (k, v) in base_env() {
            c.env(k, v);
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "wtctl {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn repo_arg(&self) -> String {
        self.repo.to_string_lossy().into_owned()
    }

    fn create(&self, slug: &str) -> (String, PathBuf) {
        let out: Value = serde_json::from_str(&self.ok(&[
            "create",
            "--repo",
            &self.repo_arg(),
            "--slug",
            slug,
            "--task",
            "example task",
            "--json",
        ]))
        .unwrap();
        (
            out["task_id"].as_str().unwrap().to_string(),
            PathBuf::from(out["worktree"].as_str().unwrap()),
        )
    }

    fn admin(&self, id: &str) -> PathBuf {
        self.repo.join(".git/worktrees").join(id)
    }

    fn marker(&self, id: &str) -> Value {
        serde_json::from_slice(&fs::read(self.admin(id).join("agent-task.json")).unwrap()).unwrap()
    }

    fn kill_owner(&self, id: &str) {
        let pid = absent_pid();
        let mut m = self.marker(id);
        m["owner"]["pid"] = pid.into();
        fs::write(
            self.admin(id).join("agent-task.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
    }

    fn list(&self, mode: &str) -> Vec<Value> {
        let out = self.ok(&["list", "--repo", &self.repo_arg(), mode, "--json"]);
        serde_json::from_str::<Value>(&out)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn main_oid(&self) -> String {
        git(&self.repo, &["rev-parse", "refs/heads/main"])
    }
}

/// A pid with no process right now: a reaped child's pid, confirmed by ESRCH.
fn absent_pid() -> u32 {
    loop {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // SAFETY: signal 0 only probes for existence.
        let gone = unsafe { libc::kill(pid as i32, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            return pid;
        }
    }
}

fn ids(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn create_records_owner_and_locks_detached_worktree() {
    let env = Env::new();
    let (id, path) = env.create("add-feature");
    assert!(id.starts_with("add-feature-"));
    assert!(path.starts_with(env.tmp.join("state/proj/worktrees")));
    let m = env.marker(&id);
    assert_eq!(m["tool"], "wtctl");
    assert_eq!(
        m["owner"]["pid"].as_u64().unwrap(),
        std::process::id() as u64
    );
    assert_eq!(m["base"].as_str().unwrap(), env.main_oid());
    assert_eq!(m["task"], "example task");
    let locked = fs::read_to_string(env.admin(&id).join("locked")).unwrap();
    assert_eq!(locked.trim(), format!("agent-task {id}"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), env.main_oid());
    assert!(
        git_cmd(&path, &["symbolic-ref", "-q", "HEAD"])
            .output()
            .unwrap()
            .status
            .code()
            == Some(1)
    );
}

#[test]
fn stale_session_variables_from_other_clients_are_ignored() {
    let env = Env::new();
    let out = env
        .cmd(&[
            "create",
            "--repo",
            &env.repo_arg(),
            "--slug",
            "two-clients",
            "--json",
        ])
        .env(
            "CLAUDE_CODE_SESSION_ID",
            "0e7f96a6-1234-4abc-8def-0123456789ab",
        )
        .env("OPENCODE_SESSION_ID", "ses_stalevaluefromadeadsession")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let m: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        m["owner"]["pid"].as_u64().unwrap(),
        std::process::id() as u64
    );
}

#[test]
fn create_refuses_without_agent_ancestor_and_leaves_nothing() {
    let env = Env::new();
    let out = env
        .cmd(&["create", "--repo", &env.repo_arg(), "--slug", "no-owner"])
        .env("WTCTL_AGENT_NAMES", "no-such-agent-name")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert!(!env.tmp.join("state/proj/worktrees").exists());
    assert_eq!(
        git(&env.repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn whoami_resolves_task_from_path_and_cwd() {
    let env = Env::new();
    let (id, path) = env.create("find-me");
    fs::create_dir_all(path.join("sub/dir")).unwrap();
    let sub = path.join("sub/dir");
    let out = env.ok(&["whoami", "--path", &sub.to_string_lossy()]);
    assert!(out.contains(&format!("task_id={id}")), "{out}");
    assert!(out.contains("mine=yes"), "{out}");
    let out = env
        .cmd(&["whoami", "--json"])
        .current_dir(&sub)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["task_id"], id.as_str());
    let main = env.run(&["whoami", "--path", &env.repo_arg()]);
    assert_eq!(main.status.code(), Some(1));
}

#[test]
fn list_separates_live_and_dead_owners() {
    let env = Env::new();
    let (live, _) = env.create("live-task");
    let (dead, _) = env.create("dead-task");
    env.kill_owner(&dead);
    assert_eq!(ids(&env.list("--orphans")), vec![dead.clone()]);
    assert_eq!(ids(&env.list("--mine")), vec![live.clone()]);
    let all = env.list("--all");
    assert_eq!(all.len(), 2);
    let row = all.iter().find(|r| r["id"] == dead.as_str()).unwrap();
    assert_eq!(row["owner_state"], "dead");
    assert_eq!(row["integrated"], true);
    assert_eq!(row["clean"], true);
    // A reused pid with a different start time is not the recorded owner.
    let mut m = env.marker(&live);
    m["owner"]["start"] = "1.000000".into();
    fs::write(
        env.admin(&live).join("agent-task.json"),
        serde_json::to_vec(&m).unwrap(),
    )
    .unwrap();
    assert_eq!(ids(&env.list("--orphans")).len(), 2);
}

#[test]
fn reap_removes_only_integrated_clean_orphans() {
    let env = Env::new();
    let (done, done_path) = env.create("done-task");
    let (ahead, ahead_path) = env.create("ahead-task");
    let (dirty, dirty_path) = env.create("dirty-task");
    let (data, data_path) = env.create("data-task");
    let (live, live_path) = env.create("live-task");
    commit_file(&ahead_path, "new.txt", "x\n");
    fs::write(dirty_path.join("scratch.txt"), "x").unwrap();
    fs::write(data_path.join("local.db"), "x").unwrap();
    fs::create_dir_all(done_path.join("__pycache__")).unwrap();
    fs::write(done_path.join("__pycache__/m.pyc"), "x").unwrap();
    for id in [&done, &ahead, &dirty, &data] {
        env.kill_owner(id);
    }
    let dry = env.ok(&[
        "reap",
        "--repo",
        &env.repo_arg(),
        "--grace-secs",
        "0",
        "--dry-run",
    ]);
    assert!(dry.contains(&format!("would-reap\t{done}")), "{dry}");
    assert!(done_path.exists());
    let out = env.ok(&["reap", "--repo", &env.repo_arg(), "--grace-secs", "0"]);
    assert!(out.contains(&format!("reaped\t{done}")), "{out}");
    for id in [&ahead, &dirty, &data] {
        assert!(out.contains(&format!("orphan\t{id}")), "{out}");
    }
    assert!(!done_path.exists());
    assert!(ahead_path.exists() && dirty_path.exists() && data_path.exists() && live_path.exists());
    assert!(!out.contains(&live));
    // Preserved orphans stay locked against plain removal.
    assert!(env.admin(&ahead).join("locked").exists());
}

#[test]
fn adopt_requires_a_dead_owner() {
    let env = Env::new();
    let (id, _) = env.create("adopt-me");
    let out = env.run(&["adopt", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    env.kill_owner(&id);
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "a dead owner's task is not ours to remove"
    );
    env.ok(&["adopt", "--repo", &env.repo_arg(), "--id", &id]);
    let m = env.marker(&id);
    assert_eq!(
        m["owner"]["pid"].as_u64().unwrap(),
        std::process::id() as u64
    );
    assert_eq!(m["history"][0]["event"], "adopt");
    env.ok(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
}

#[test]
fn integrate_fast_forwards_checked_out_main_and_audits() {
    let env = Env::new();
    let (id, path) = env.create("ff-task");
    let tip = commit_file(&path, "g.txt", "task\n");
    let out = env.ok(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(out.contains("integrated"), "{out}");
    assert_eq!(env.main_oid(), tip);
    assert_eq!(
        fs::read_to_string(env.repo.join("g.txt")).unwrap(),
        "task\n"
    );
    let audit = fs::read_to_string(env.repo.join(".git/agent-audit.jsonl")).unwrap();
    assert!(audit.contains(&id));
    let again = env.ok(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(again.contains("already integrated"));
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!path.exists());
}

#[test]
fn integrate_uses_cas_when_main_is_not_checked_out() {
    let env = Env::new();
    git(&env.repo, &["switch", "-q", "-c", "other"]);
    let (id, path) = env.create("cas-task");
    let tip = commit_file(&path, "g.txt", "task\n");
    env.ok(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(env.main_oid(), tip);
}

#[test]
fn integrate_reports_conflicts_when_main_diverged() {
    let env = Env::new();
    let (id, path) = env.create("conflict-task");
    commit_file(&path, "f.txt", "task side\n");
    let main_tip = commit_file(&env.repo, "f.txt", "main side\n");
    let out = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("conflict\tf.txt"));
    assert_eq!(env.main_oid(), main_tip);
}

#[test]
fn integrate_refuses_dirty_task_and_foreign_owner() {
    let env = Env::new();
    let (id, path) = env.create("dirty-int");
    commit_file(&path, "g.txt", "x\n");
    fs::write(path.join("g.txt"), "changed\n").unwrap();
    let out = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    git(&path, &["commit", "-qam", "fix"]);
    env.kill_owner(&id);
    let out = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
}

#[test]
fn integration_lock_is_exclusive_and_honors_legacy_lock() {
    let env = Env::new();
    let (id, path) = env.create("lock-task");
    commit_file(&path, "g.txt", "x\n");
    let lock = env.repo.join(".git/agent-locks/integrate.lock");
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let held = fs::File::create(&lock).unwrap();
    // SAFETY: valid descriptor owned by `held`.
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let out = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(5));
    drop(held);
    let legacy = env.repo.join(".git/main-integration.lock");
    fs::create_dir(&legacy).unwrap();
    let out = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(5));
    fs::remove_dir(&legacy).unwrap();
    env.ok(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
}

#[test]
fn legacy_owner_metadata_is_listed_read_only() {
    let env = Env::new();
    let path = env.tmp.join("state/proj/worktrees/old-task-20260101-01");
    git(
        &env.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &path.to_string_lossy(),
            "HEAD",
        ],
    );
    let base = env.main_oid();
    fs::write(
        env.repo.join(".git/worktrees/old-task-20260101-01/agent-task"),
        format!(
            "old task client=claude session=0e7f96a6-1234-4abc-8def-0123456789ab base={base} pid={}@1\n",
            absent_pid()
        ),
    )
    .unwrap();
    let all = env.list("--all");
    let row = all.iter().find(|r| r["kind"] == "legacy").unwrap();
    assert_eq!(row["id"], "old-task-20260101-01");
    assert_eq!(row["owner"], "claude:0e7f96a6-1234-4abc-8def-0123456789ab");
    assert_eq!(row["owner_state"], "dead");
    assert!(
        env.list("--orphans").is_empty(),
        "legacy entries are never adopt candidates"
    );
    let out = env.run(&["whoami", "--path", &path.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("legacy"));
    let out = env.run(&["reap", "--repo", &env.repo_arg(), "--grace-secs", "0"]);
    assert!(out.status.success());
    assert!(path.exists());
}

fn reap(env: &Env) -> String {
    env.ok(&["reap", "--repo", &env.repo_arg(), "--grace-secs", "0"])
}

fn write_hook(env: &Env, name: &str) {
    let hooks = env.repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let path = hooks.join(name);
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn reflog_only_commits_block_reap_and_remove() {
    let env = Env::new();
    let (id, path) = env.create("reset-away");
    commit_file(&path, "lost.txt", "work\n");
    git(&path, &["reset", "-q", "--hard", "HEAD~1"]);
    let row = env
        .list("--all")
        .into_iter()
        .find(|r| r["id"] == id.as_str())
        .unwrap();
    assert_eq!(row["unintegrated"], 1, "{row}");
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&out.stderr).contains("reflog"));
    env.kill_owner(&id);
    let out = reap(&env);
    assert!(out.contains(&format!("orphan\t{id}")), "{out}");
    assert!(path.exists());
}

#[test]
fn in_progress_rebase_blocks_reap() {
    let env = Env::new();
    let (id, path) = env.create("mid-rebase");
    commit_file(&path, "f.txt", "task\n");
    commit_file(&env.repo, "f.txt", "main\n");
    let rebase = git_cmd(&path, &["rebase", "-q", "refs/heads/main"])
        .output()
        .unwrap();
    assert!(!rebase.status.success());
    assert!(env.admin(&id).join("rebase-merge").exists());
    env.kill_owner(&id);
    let out = reap(&env);
    assert!(out.contains("rebase-merge in progress"), "{out}");
    assert!(path.exists());
}

#[test]
fn skip_worktree_edits_block_reap_remove_and_integrate() {
    let env = Env::new();
    let (id, path) = env.create("hidden-edit");
    git(&path, &["update-index", "--skip-worktree", "f.txt"]);
    fs::write(path.join("f.txt"), "hidden change\n").unwrap();
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    let out = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    env.kill_owner(&id);
    let out = reap(&env);
    assert!(out.contains("skip-worktree"), "{out}");
    assert!(path.exists());
}

#[test]
fn reap_honors_grace_period_and_keeps_going_after_a_failure() {
    let env = Env::new();
    let (young, young_path) = env.create("young-task");
    env.kill_owner(&young);
    let out = env.ok(&["reap", "--repo", &env.repo_arg()]);
    assert!(out.contains("grace period"), "{out}");
    assert!(young_path.exists());
    let (other, other_path) = env.create("other-task");
    env.kill_owner(&other);
    // Running from inside one candidate makes its removal fail; the other is still reaped.
    let out = env
        .cmd(&["reap", "--repo", &env.repo_arg(), "--grace-secs", "0"])
        .current_dir(&young_path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(&format!("orphan\t{young}\tremoval failed")),
        "{text}"
    );
    assert!(text.contains(&format!("reaped\t{other}")), "{text}");
    assert!(young_path.exists() && !other_path.exists());
    assert!(env.admin(&young).join("locked").exists());
}

#[test]
fn mismatched_marker_is_invalid_everywhere() {
    let env = Env::new();
    let (id, path) = env.create("moved-marker");
    env.kill_owner(&id);
    let mut m = env.marker(&id);
    m["worktree"] = env
        .tmp
        .join("elsewhere")
        .to_string_lossy()
        .into_owned()
        .into();
    fs::write(
        env.admin(&id).join("agent-task.json"),
        serde_json::to_vec(&m).unwrap(),
    )
    .unwrap();
    assert!(env.list("--orphans").is_empty());
    let row = env.list("--all").into_iter().next().unwrap();
    assert_eq!(row["kind"], "invalid");
    assert!(reap(&env).contains("invalid marker"));
    let out = env.run(&["adopt", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(1));
    assert!(path.exists());
}

#[test]
fn integrate_takes_and_releases_the_legacy_lock() {
    let env = Env::new();
    let (id, path) = env.create("legacy-lock");
    commit_file(&path, "g.txt", "x\n");
    // Leftovers from an earlier release are ignored, as in the legacy protocol.
    fs::create_dir(env.repo.join(".git/main-integration.lock.released.stale")).unwrap();
    env.ok(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    let names: Vec<String> = fs::read_dir(env.repo.join(".git"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("main-integration.lock"))
        .collect();
    assert_eq!(
        names,
        vec!["main-integration.lock.released.stale".to_string()]
    );
}

#[test]
fn repository_hooks_block_until_allowed() {
    let env = Env::new();
    write_hook(&env, "post-checkout");
    let out = env.run(&["create", "--repo", &env.repo_arg(), "--slug", "hook-task"]);
    assert_eq!(out.status.code(), Some(4));
    assert!(!env.tmp.join("state/proj/worktrees").exists());
    env.ok(&[
        "create",
        "--repo",
        &env.repo_arg(),
        "--slug",
        "hook-task",
        "--allow-repository-hooks",
    ]);
    let (id, path) = (
        env.list("--all")[0]["id"].as_str().unwrap().to_string(),
        PathBuf::from(env.list("--all")[0]["path"].as_str().unwrap()),
    );
    commit_file(&path, "g.txt", "x\n");
    write_hook(&env, "post-merge");
    let out = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    write_hook(&env, "reference-transaction");
    let out = env.run(&[
        "integrate",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--allow-repository-hooks",
    ]);
    assert_eq!(out.status.code(), Some(4));
    fs::remove_file(env.repo.join(".git/hooks/reference-transaction")).unwrap();
    env.ok(&[
        "integrate",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--allow-repository-hooks",
    ]);
}

#[test]
fn list_bounds_parallel_children_under_a_low_fd_limit() {
    let env = Env::new();
    let root = env.tmp.join("many");
    for i in 0..60 {
        let p = root.join(format!("wt{i}"));
        git(
            &env.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &p.to_string_lossy(),
                "HEAD",
            ],
        );
    }
    let script = format!(
        "ulimit -n 96 && exec '{}' list --repo '{}' --all --json",
        env!("CARGO_BIN_EXE_wtctl"),
        env.repo_arg()
    );
    let cmd = env.cmd(&[]);
    let out = Command::new("/bin/sh")
        .args(["-c", &script])
        .envs(cmd.get_envs().filter_map(|(k, v)| Some((k, v?))))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 60);
    assert!(rows
        .iter()
        .all(|r| r["clean"] == true && r["integrated"] == true));
}

#[test]
fn rejects_bad_branch_names_and_paths_without_panicking() {
    let env = Env::new();
    let out = env.run(&["list", "--repo", &env.repo_arg(), "--main", "bad..name"]);
    assert_eq!(out.status.code(), Some(2));
    let bad = OsStr::from_bytes(b"/nonexistent/\xff\xfe");
    let out = env.cmd(&["whoami", "--path"]).arg(bad).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let out = env.cmd(&["list", "--repo"]).arg(bad).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn state_home_must_be_absolute() {
    let env = Env::new();
    let out = env
        .cmd(&[
            "create",
            "--repo",
            &env.repo_arg(),
            "--slug",
            "rel-state",
            "--json",
        ])
        .env("XDG_STATE_HOME", "relative/state")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["worktree"]
        .as_str()
        .unwrap()
        .starts_with(&*env.tmp.join("home/.local/state").to_string_lossy()));
    let out = env
        .cmd(&["create", "--repo", &env.repo_arg(), "--slug", "no-home"])
        .env("XDG_STATE_HOME", "relative")
        .env("HOME", "")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn per_worktree_ref_commits_block_remove_and_reap() {
    let env = Env::new();
    let (id, path) = env.create("worktree-ref");
    let tree = git(&path, &["rev-parse", "HEAD^{tree}"]);
    let lone = git(
        &path,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "only a ref"],
    );
    git(&path, &["update-ref", "refs/worktree/keep", &lone]);
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("per-worktree"),
        "{out:?}"
    );
    env.kill_owner(&id);
    let out = reap(&env);
    assert!(out.contains(&format!("orphan\t{id}")), "{out}");
    assert!(path.exists());
}

#[test]
fn missing_reflog_fails_closed() {
    let env = Env::new();
    let (id, path) = env.create("no-reflog");
    fs::remove_file(env.admin(&id).join("logs/HEAD")).unwrap();
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("reflog unavailable"),
        "{out:?}"
    );
    assert!(path.exists());
}

#[test]
fn processes_inside_block_remove_and_reap() {
    let env = Env::new();
    let (id, path) = env.create("busy-task");
    let mut sleeper = Command::new("sleep")
        .arg("30")
        .current_dir(&path)
        .spawn()
        .unwrap();
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    env.kill_owner(&id);
    let reaped = reap(&env);
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert_eq!(out.status.code(), Some(4), "{err}");
    assert!(err.contains(&sleeper.id().to_string()), "{err}");
    assert!(reaped.contains(&format!("orphan\t{id}")), "{reaped}");
    assert!(path.exists());
}

#[test]
fn foreign_lock_reason_is_respected() {
    let env = Env::new();
    let (id, path) = env.create("human-hold");
    let p = path.to_string_lossy().into_owned();
    git(&env.repo, &["worktree", "unlock", &p]);
    git(
        &env.repo,
        &["worktree", "lock", "--reason", "held by a person", &p],
    );
    let out = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(out.status.code(), Some(4));
    let reason = fs::read_to_string(env.admin(&id).join("locked")).unwrap();
    assert_eq!(reason.trim(), "held by a person");
    assert!(path.exists());
}

#[test]
fn failed_removal_relocks_with_the_original_reason() {
    let env = Env::new();
    let (id, path) = env.create("relock-task");
    let real = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
    let shim_dir = env.tmp.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = worktree ] && [ \"$a\" = remove ]; then exit 1; fi\n  prev=$a\ndone\nexec '{real}' \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let path_var = format!(
        "{}:{}",
        shim_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = env
        .cmd(&["remove", "--repo", &env.repo_arg(), "--id", &id])
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reason = fs::read_to_string(env.admin(&id).join("locked")).unwrap();
    assert_eq!(reason.trim(), format!("agent-task {id}"));
    assert!(path.exists());
}

#[test]
fn sigterm_during_integration_still_releases_the_legacy_lock() {
    let env = Env::new();
    let (id, path) = env.create("signal-task");
    let tip = commit_file(&path, "g.txt", "x\n");
    let mark = env.tmp.join("hook-started");
    let hooks = env.repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-merge");
    fs::write(&hook, "#!/bin/sh\ntouch \"$WTCTL_TEST_MARK\"\nsleep 2\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = env
        .cmd(&[
            "integrate",
            "--repo",
            &env.repo_arg(),
            "--id",
            &id,
            "--allow-repository-hooks",
        ])
        .env("WTCTL_TEST_MARK", &mark)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&mark);
    let lock = env.repo.join(".git/main-integration.lock");
    let owner = fs::read_to_string(lock.join("owner")).unwrap();
    let token = owner.lines().next().unwrap();
    assert_eq!(token.len(), 32);
    assert!(owner.lines().nth(1) == Some(&child.id().to_string()));
    // SAFETY: plain kill(2) on our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert!(!lock.exists());
    assert!(!env
        .repo
        .join(format!(".git/main-integration.lock.released.{token}"))
        .exists());
    assert_eq!(env.main_oid(), tip);
}

#[test]
fn missing_claim_time_counts_as_recent() {
    let env = Env::new();
    let (id, path) = env.create("no-claim");
    env.kill_owner(&id);
    let mut m = env.marker(&id);
    m.as_object_mut().unwrap().remove("created_unix");
    fs::write(
        env.admin(&id).join("agent-task.json"),
        serde_json::to_vec(&m).unwrap(),
    )
    .unwrap();
    assert!(reap(&env).contains("grace period"));
    assert!(path.exists());
}

#[test]
fn relative_branch_syntax_is_rejected_for_main() {
    let env = Env::new();
    for bad in ["@{-1}", "@", "-x"] {
        let out = env.run(&["list", "--repo", &env.repo_arg(), &format!("--main={bad}")]);
        assert_eq!(out.status.code(), Some(2), "{bad}");
    }
}

#[test]
fn relative_gitdir_worktrees_resolve_against_the_admin_dir() {
    let env = Env::new();
    let p = env.tmp.join("rel-wt");
    git(
        &env.repo,
        &[
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            "-q",
            "--detach",
            &p.to_string_lossy(),
            "HEAD",
        ],
    );
    let gitdir = fs::read_to_string(env.repo.join(".git/worktrees/rel-wt/gitdir")).unwrap();
    assert!(!gitdir.starts_with('/'), "{gitdir}");
    let row = env.list("--all").into_iter().next().unwrap();
    assert_eq!(
        row["path"],
        fs::canonicalize(&p).unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(row["exists"], true);
}

/// A PATH whose `git` runs `hook` (shell, with `$sub`/`$verb` set) before the real git.
fn git_shim(env: &Env, name: &str, hook: &str) -> String {
    let real = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
    let dir = env.tmp.join(name);
    fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nsub=\nverb=\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = worktree ]; then sub=worktree; verb=$a; fi\n  prev=$a\ndone\n{hook}\nexec '{real}' \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[test]
fn failed_relock_reports_an_unlocked_worktree() {
    let env = Env::new();
    let (id, path) = env.create("unlocked-task");
    let path_var = git_shim(
        &env,
        "shim-fail",
        "if [ \"$sub\" = worktree ] && { [ \"$verb\" = remove ] || [ \"$verb\" = lock ]; }; then exit 1; fi",
    );
    let out = env
        .cmd(&["remove", "--repo", &env.repo_arg(), "--id", &id])
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("UNLOCKED"),
        "{out:?}"
    );
    assert!(path.exists());
    assert!(!env.admin(&id).join("locked").exists());
}

#[test]
fn reap_stops_starting_removals_after_a_signal() {
    let env = Env::new();
    let (a, _) = env.create("first-task");
    let (b, _) = env.create("second-task");
    let (c, _) = env.create("third-task");
    for id in [&a, &b, &c] {
        env.kill_owner(id);
    }
    let mark = env.tmp.join("removing");
    let path_var = git_shim(
        &env,
        "shim-slow",
        "if [ \"$sub\" = worktree ] && [ \"$verb\" = remove ]; then touch \"$WTCTL_TEST_MARK\"; sleep 1; fi",
    );
    let child = env
        .cmd(&["reap", "--repo", &env.repo_arg(), "--grace-secs", "0"])
        .env("PATH", path_var)
        .env("WTCTL_TEST_MARK", &mark)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&mark);
    // SAFETY: plain kill(2) on our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.signal(), Some(libc::SIGTERM), "{text}");
    assert_eq!(text.matches("reaped\t").count(), 1, "{text}");
    assert_eq!(text.matches("kept (interrupted)").count(), 2, "{text}");
    assert!(!env.repo.join(".git/main-integration.lock").exists());
}

#[test]
fn interrupted_failed_removal_still_reports_the_unlocked_worktree() {
    let env = Env::new();
    let (id, path) = env.create("signal-unlock");
    let mark = env.tmp.join("removing");
    let path_var = git_shim(
        &env,
        "shim-slow-fail",
        "if [ \"$sub\" = worktree ] && [ \"$verb\" = remove ]; then touch \"$WTCTL_TEST_MARK\"; sleep 1; exit 1; fi\nif [ \"$sub\" = worktree ] && [ \"$verb\" = lock ]; then exit 1; fi",
    );
    let child = env
        .cmd(&["remove", "--repo", &env.repo_arg(), "--id", &id])
        .env("PATH", path_var)
        .env("WTCTL_TEST_MARK", &mark)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&mark);
    // SAFETY: plain kill(2) on our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.signal(), Some(libc::SIGINT), "{err}");
    assert!(err.contains("UNLOCKED"), "{err}");
    assert!(path.exists());
    assert!(!env.repo.join(".git/main-integration.lock").exists());
}

#[test]
fn non_ascii_cwds_are_judged_by_their_real_path() {
    let env = Env::new();
    let (id, path) = env.create("wide-cwd");
    let outside = env.tmp.join("外部");
    let inside = path.join("日本語");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&inside).unwrap();
    let mut away = Command::new("sleep")
        .arg("30")
        .current_dir(&outside)
        .spawn()
        .unwrap();
    let mut within = Command::new("sleep")
        .arg("30")
        .current_dir(&inside)
        .spawn()
        .unwrap();
    let refused = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    within.kill().unwrap();
    within.wait().unwrap();
    let removed = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    away.kill().unwrap();
    away.wait().unwrap();
    let err = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(4), "{err}");
    assert!(err.contains(&within.id().to_string()), "{err}");
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!path.exists());
}
