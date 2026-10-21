//! End-to-end tests against throwaway repositories with real Git.
//!
//! The test binary itself plays the coding agent: `WTCTL_AGENT_NAMES` (only
//! honored with the `test-agent-names` feature) names it, so every wtctl
//! child finds this process as its owning ancestor.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
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

fn add_annex_filter_backed_file(env: &Env, name: &str) -> String {
    git(&env.repo, &["annex", "init", "-q"]);
    fs::write(
        env.repo.join(".gitattributes"),
        format!("{name} annex.largefiles=anything\n"),
    )
    .unwrap();
    git(&env.repo, &["add", ".gitattributes"]);
    git(&env.repo, &["commit", "-q", "-m", "configure annex filter"]);
    fs::create_dir_all(env.repo.join(name).parent().unwrap()).unwrap();
    fs::write(env.repo.join(name), vec![b'x'; 8192]).unwrap();
    git(&env.repo, &["add", "--", name]);
    let stage = git(&env.repo, &["ls-files", "--stage", "--", name]);
    let fields: Vec<_> = stage.split_whitespace().collect();
    assert_eq!(fields.len(), 4, "unexpected index entry: {stage}");
    assert_eq!(
        fields[0], "100644",
        "annex filter must materialize a regular file"
    );
    let raw = git(&env.repo, &["hash-object", "--no-filters", "--", name]);
    let filtered = git(
        &env.repo,
        &[
            "hash-object",
            "--filters",
            &format!("--path={name}"),
            "--",
            name,
        ],
    );
    assert_ne!(
        raw, fields[1],
        "test fixture must differ without the annex filter"
    );
    assert_eq!(
        filtered, fields[1],
        "annex clean filter must match the index blob"
    );
    git(&env.repo, &["commit", "-q", "-m", "annexed file"]);
    fields[1].to_string()
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

    fn allow_main_worktree_symlinks(&self, names: &[&str]) {
        let names = serde_json::to_string(names).unwrap();
        let config = format!(
            r#"{{"schema_version":1,"generated_artifacts":[],"main_worktree_symlinks":{names}}}"#
        );
        fs::write(self.repo.join(".wtctl.json"), config).unwrap();
        git(&self.repo, &["add", ".wtctl.json"]);
        git(
            &self.repo,
            &["commit", "-q", "-m", "configure main-worktree symlinks"],
        );
    }

    fn raw_cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wtctl"));
        c.args(args)
            .env("HOME", self.tmp.join("home"))
            .env("XDG_STATE_HOME", self.tmp.join("state"))
            .env("WTCTL_AGENT_NAMES", &self.agent)
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("OPENCODE_SESSION_ID")
            .env_remove("CLAUDE_CONFIG_DIR")
            .current_dir(&self.tmp);
        for (k, v) in base_env() {
            c.env(k, v);
        }
        c
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut command = self.raw_cmd(args);
        if args
            .first()
            .is_some_and(|command| matches!(*command, "integrate" | "remove" | "reap"))
            && !args.contains(&"--external-writers-quiesced")
        {
            command.arg("--external-writers-quiesced");
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "wtctl {args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
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

    fn create_annex(&self, slug: &str) -> (String, PathBuf) {
        let out: Value = serde_json::from_str(&self.ok(&[
            "create",
            "--repo",
            &self.repo_arg(),
            "--slug",
            slug,
            "--task",
            "example annex task",
            "--external-writers-quiesced",
            "--allow-repository-hooks",
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

    fn exclude(&self, pattern: &str) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.repo.join(".git/info/exclude"))
            .unwrap();
        writeln!(file, "{pattern}").unwrap();
    }

    fn quarantine_root(&self) -> PathBuf {
        self.tmp.join("state/proj/worktrees/.removal-quarantine")
    }
}

fn write_stale_nix_direnv_cache(worktree: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::symlink;

    let cache = worktree.join(".direnv");
    fs::create_dir_all(cache.join("bin")).unwrap();
    fs::create_dir_all(cache.join("flake-inputs")).unwrap();
    let input_hash = "a".repeat(32);
    let profile_hash = "b".repeat(32);
    let input_name = format!("{input_hash}-source");
    let input_target = Path::new("/nix/store").join(&input_name);
    let profile_target =
        PathBuf::from("/nix/store").join(format!("{}-nix-shell-env", "c".repeat(32)));
    assert_eq!(
        fs::symlink_metadata(&input_target).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        fs::symlink_metadata(&profile_target).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );

    let reload = cache.join("bin/nix-direnv-reload");
    fs::write(&reload, "reload").unwrap();
    fs::set_permissions(&reload, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&input_target, cache.join("flake-inputs").join(&input_name)).unwrap();
    symlink(
        profile_target.clone(),
        cache.join(format!("flake-profile-{profile_hash}")),
    )
    .unwrap();
    fs::write(
        cache.join(format!("flake-profile-{profile_hash}.rc")),
        "profile",
    )
    .unwrap();
    (input_target, profile_target)
}

#[test]
fn capabilities_reports_the_lifecycle_contract() {
    let env = Env::new();
    let output: Value = serde_json::from_str(&env.ok(&["capabilities", "--json"])).unwrap();
    assert_eq!(output["schema"], 1);
    assert_eq!(output["finish"], true);
    assert_eq!(output["legacy_session_lifecycle"], true);
    assert_eq!(output["git_annex"], true);
    assert_eq!(output["artifact_quarantine"], true);
    assert_eq!(output["quarantine_purge"], true);
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
fn create_supports_parent_metadata_and_a_custom_worktrees_root() {
    let env = Env::new();
    let root = env.tmp.join("custom-worktrees");
    let output: Value = serde_json::from_str(&env.ok(&[
        "create",
        "--repo",
        &env.repo_arg(),
        "--slug",
        "custom-root",
        "--task",
        "revision task",
        "--parent",
        "parent-task-id",
        "--worktrees-root",
        &root.to_string_lossy(),
        "--json",
    ]))
    .unwrap();
    let id = output["task_id"].as_str().unwrap();
    let path = PathBuf::from(output["worktree"].as_str().unwrap());
    assert!(path.starts_with(&root));
    assert_eq!(env.marker(id)["parent"], "parent-task-id");
}

#[test]
fn remove_requires_explicit_selection_for_build_outputs_and_discards_after_success() {
    let env = Env::new();
    let (id, path) = env.create("remove-build-artifact");
    env.exclude("/target/");
    fs::create_dir_all(path.join("target/debug")).unwrap();
    fs::write(path.join("target/debug/artifact"), b"generated").unwrap();

    let refused = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(path.join("target/debug/artifact").exists());

    let removed = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--discard-generated-build-outputs",
    ]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!path.exists());
    let quarantine = env.quarantine_root();
    assert!(quarantine.exists());
    assert!(quarantine.read_dir().unwrap().next().is_none());
}

#[test]
fn remove_dry_run_reports_selected_artifacts_without_mutating_them() {
    let env = Env::new();
    let (id, path) = env.create("dry-run-artifact");
    env.exclude("/target/");
    fs::create_dir_all(path.join("target/debug")).unwrap();
    fs::write(path.join("target/debug/artifact"), b"keep during dry run").unwrap();
    let output = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--discard-generated-build-outputs",
        "--dry-run",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("generated artifact(s)"));
    assert!(path.join("target/debug/artifact").exists());
    assert!(!env.quarantine_root().exists());
}

#[test]
fn remove_json_emits_one_structured_result() {
    let env = Env::new();
    let (id, path) = env.create("remove-json");
    let output = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id, "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "removed");
    assert_eq!(result["worktree_id"], id);
    assert!(!path.exists());
}

#[test]
fn failed_worktree_removal_restores_quarantined_generated_artifacts() {
    let env = Env::new();
    let (id, path) = env.create("restore-generated-cache");
    env.exclude("/target/");
    fs::create_dir_all(path.join("target/debug")).unwrap();
    fs::write(path.join("target/debug/artifact"), b"keep after failure").unwrap();
    let path_var = git_shim(
        &env,
        "fail-generated-remove",
        "if [ \"$sub\" = worktree ] && [ \"$verb\" = remove ]; then exit 1; fi",
    );
    let out = env
        .cmd(&[
            "remove",
            "--repo",
            &env.repo_arg(),
            "--id",
            &id,
            "--discard-generated-build-outputs",
        ])
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("git failed:"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(path.join("target/debug/artifact")).unwrap(),
        b"keep after failure"
    );
    let quarantine = env.quarantine_root();
    assert!(
        quarantine.exists() && quarantine.read_dir().unwrap().next().is_none(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn finish_discards_default_generated_artifacts_after_integration() {
    let env = Env::new();
    let (id, path) = env.create("finish-build-artifact");
    env.exclude("/target/");
    fs::create_dir_all(path.join("target/debug")).unwrap();
    fs::write(path.join("target/debug/artifact"), b"generated").unwrap();
    let finished = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
    ]);
    assert!(
        finished.status.success(),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(!path.exists());
    assert!(env.quarantine_root().exists());
    assert!(env.quarantine_root().read_dir().unwrap().next().is_none());
}

#[test]
fn discarding_result_removes_only_the_symlink() {
    use std::os::unix::fs::symlink;

    let env = Env::new();
    let (id, path) = env.create("result-link");
    env.exclude("/result");
    let external = env.tmp.join("external-build-output");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("keep"), b"external output").unwrap();
    symlink(&external, path.join("result")).unwrap();
    let removed = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--discard-generated-build-outputs",
    ]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!path.exists());
    assert_eq!(fs::read(external.join("keep")).unwrap(), b"external output");
}

#[test]
fn selected_build_root_cannot_be_a_symlink_to_external_data() {
    use std::os::unix::fs::symlink;

    let env = Env::new();
    let (id, path) = env.create("unsafe-build-link");
    env.exclude("/target/");
    let external = env.tmp.join("external-target");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("keep"), b"user data").unwrap();
    symlink(&external, path.join("target")).unwrap();
    let refused = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--discard-generated-build-outputs",
    ]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(path.is_dir());
    assert!(path.join("target").is_symlink());
    assert_eq!(fs::read(external.join("keep")).unwrap(), b"user data");
}

#[test]
fn preserved_generated_artifacts_can_be_purged_by_exact_operation_id() {
    let env = Env::new();
    let (id, path) = env.create("preserve-cache");
    env.exclude("/.ruff_cache/");
    fs::create_dir_all(path.join(".ruff_cache")).unwrap();
    fs::write(path.join(".ruff_cache/state"), b"cache state").unwrap();
    let removed = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--preserve-generated-artifacts",
    ]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!path.exists());
    let mut operations = env.quarantine_root().read_dir().unwrap();
    let operation = operations
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    assert!(operations.next().is_none());
    let cache = env
        .quarantine_root()
        .join(&operation)
        .join("generated-cache-000/state");
    assert_eq!(fs::read(&cache).unwrap(), b"cache state");

    let purged = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--purge-preserved-quarantine",
        &operation,
    ]);
    assert!(
        purged.status.success(),
        "{}",
        String::from_utf8_lossy(&purged.stderr)
    );
    assert!(!env.quarantine_root().join(operation).exists());
}

#[test]
fn preserved_quarantine_purge_resumes_from_its_signed_cursor() {
    let env = Env::new();
    let (id, path) = env.create("resume-cache-purge");
    env.exclude("/.ruff_cache/");
    fs::create_dir_all(path.join(".ruff_cache/nested")).unwrap();
    fs::write(path.join(".ruff_cache/nested/state"), b"cache state").unwrap();
    let removed = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--preserve-generated-artifacts",
    ]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let operation = env
        .quarantine_root()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let operation_id = operation
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let archive = operation.join("generated-cache-000");
    fs::remove_file(archive.join("nested/state")).unwrap();
    let manifest_path = operation.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["generated_caches"][0]["purge_state"] = "purging".into();
    manifest["generated_caches"][0]["purge_cursor"] = 0.into();
    manifest["generated_caches"][0]["purge_pending"] = "nested/state".into();
    manifest["phase"] = "purging_preserved".into();
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let migration_journal = operation.join("purge-generated-cache-000.jsonl");
    fs::write(&migration_journal, b"{\"partial\"").unwrap();
    fs::set_permissions(&migration_journal, fs::Permissions::from_mode(0o600)).unwrap();

    let purged = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--purge-preserved-quarantine",
        &operation_id,
    ]);
    assert!(
        purged.status.success(),
        "{}",
        String::from_utf8_lossy(&purged.stderr)
    );
    assert!(!operation.exists());
}

#[test]
fn unknown_direnv_is_quarantined_and_requires_an_explicit_unverified_purge() {
    let env = Env::new();
    let (id, path) = env.create("preserve-direnv");
    fs::create_dir_all(path.join(".direnv/user-data")).unwrap();
    fs::write(path.join(".direnv/user-data/keep"), b"not a verified cache").unwrap();
    let removed = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!path.exists());
    let operation = env
        .quarantine_root()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let preserved = operation.join(".direnv/user-data/keep");
    assert_eq!(fs::read(&preserved).unwrap(), b"not a verified cache");
    let operation_id = operation
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let refused = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--purge-preserved-quarantine",
        &operation_id,
    ]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(preserved.exists());

    let purged = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--purge-preserved-quarantine",
        &operation_id,
        "--discard-unverified-direnv",
    ]);
    assert!(
        purged.status.success(),
        "{}",
        String::from_utf8_lossy(&purged.stderr)
    );
    assert!(!operation.exists());
}

#[test]
fn canonical_database_link_is_unlinked_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let env = Env::new();
    env.allow_main_worktree_symlinks(&["state.db"]);
    let database = env.repo.join("state.db");
    fs::write(&database, b"canonical database").unwrap();
    let (id, path) = env.create("database-link");
    symlink(&database, path.join("state.db")).unwrap();
    let removed = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!path.exists());
    assert_eq!(fs::read(database).unwrap(), b"canonical database");
}

#[test]
fn failed_removal_restores_the_canonical_database_symlink() {
    use std::os::unix::fs::symlink;

    let env = Env::new();
    env.allow_main_worktree_symlinks(&["state.db"]);
    let database = env.repo.join("state.db");
    fs::write(&database, b"canonical database").unwrap();
    let (id, path) = env.create("restore-database-link");
    symlink(&database, path.join("state.db")).unwrap();
    let path_var = git_shim(
        &env,
        "fail-database-remove",
        "if [ \"$sub\" = worktree ] && [ \"$verb\" = remove ]; then exit 1; fi",
    );
    let out = env
        .cmd(&["remove", "--repo", &env.repo_arg(), "--id", &id])
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
    assert!(path.is_dir());
    assert!(fs::symlink_metadata(path.join("state.db"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::canonicalize(path.join("state.db")).unwrap(), database);
    assert_eq!(fs::read(database).unwrap(), b"canonical database");
}

#[test]
fn multiple_matching_main_worktree_symlinks_are_refused() {
    let env = Env::new();
    env.allow_main_worktree_symlinks(&["first.db", "second.db"]);
    let first = env.repo.join("first.db");
    let second = env.repo.join("second.db");
    fs::write(&first, b"first database").unwrap();
    fs::write(&second, b"second database").unwrap();
    let (id, path) = env.create("multiple-database-links");
    symlink(&first, path.join("first.db")).unwrap();
    symlink(&second, path.join("second.db")).unwrap();

    let removed = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(!removed.status.success());
    assert!(path.is_dir());
    assert!(fs::symlink_metadata(path.join("first.db"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(fs::symlink_metadata(path.join("second.db"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(first).unwrap(), b"first database");
    assert_eq!(fs::read(second).unwrap(), b"second database");
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
fn finish_requires_writer_ack_and_integrates_then_removes_under_one_command() {
    let env = Env::new();
    let (id, path) = env.create("finish-task");
    let tip = commit_file(&path, "finish.txt", "finished\n");
    let refused = env.run(&["finish", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(refused.status.code(), Some(4));
    assert!(path.exists());
    let out = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["status"], "finished");
    assert_eq!(result["task_id"], id);
    assert_eq!(env.main_oid(), tip);
    assert_eq!(git(&env.repo, &["show", "HEAD:finish.txt"]), "finished");
    assert!(!path.exists());
    assert!(env.list("--all").is_empty());
}

#[test]
fn finish_json_errors_are_machine_readable() {
    let env = Env::new();
    let (id, path) = env.create("finish-json-error");
    let output = env.run(&["finish", "--repo", &env.repo_arg(), "--id", &id, "--json"]);
    assert_eq!(output.status.code(), Some(4));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "error");
    assert_eq!(result["exit_code"], 4);
    assert!(path.exists());
}

#[test]
fn finish_keep_worktree_integrates_without_removing_it() {
    let env = Env::new();
    let (id, path) = env.create("finish-keep");
    let tip = commit_file(&path, "kept.txt", "task\n");
    let output = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--keep-worktree",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "integrated-kept");
    assert_eq!(env.main_oid(), tip);
    assert!(path.is_dir());
    assert!(env.admin(&id).is_dir());
}

#[test]
fn finish_merges_a_compatible_main_advance_before_removing_the_task() {
    let env = Env::new();
    let (id, path) = env.create("finish-advance");
    commit_file(&path, "task.txt", "task\n");
    commit_file(&env.repo, "main.txt", "main\n");
    let out = env.ok(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
    ]);
    assert!(out.contains(&format!("finished {id}")), "{out}");
    assert_eq!(git(&env.repo, &["show", "HEAD:task.txt"]), "task");
    assert_eq!(git(&env.repo, &["show", "HEAD:main.txt"]), "main");
    assert!(!path.exists());
    assert!(env.list("--all").is_empty());
}

#[test]
fn finish_rejects_ignored_and_case_aliased_paths_before_updating_main() {
    let env = Env::new();
    let (id, path) = env.create("finish-path-collision");
    env.exclude("/ignored.txt");
    fs::write(env.repo.join("ignored.txt"), b"preserve\n").unwrap();
    let ignored_task_path = path.join("ignored.txt");
    fs::write(&ignored_task_path, b"task\n").unwrap();
    git(&path, &["add", "-f", "ignored.txt"]);
    git(&path, &["commit", "-q", "-m", "task"]);
    fs::create_dir_all(env.repo.join("Folder")).unwrap();
    let main_tip = commit_file(&env.repo, "Folder/main.txt", "main\n");
    let output = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
    ]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(
        fs::read(env.repo.join("ignored.txt")).unwrap(),
        b"preserve\n"
    );
    assert!(path.is_dir());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("ignored/untracked data"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn finish_rejects_case_aliased_merge_directories_before_creating_a_merge_commit() {
    let env = Env::new();
    let (id, path) = env.create("finish-case-alias");
    fs::create_dir_all(path.join("folder")).unwrap();
    commit_file(&path, "folder/task.txt", "task\n");
    fs::create_dir_all(env.repo.join("Folder")).unwrap();
    let main_tip = commit_file(&env.repo, "Folder/main.txt", "main\n");
    let output = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
    ]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(env.main_oid(), main_tip);
    assert!(path.is_dir());
    assert!(String::from_utf8_lossy(&output.stderr).contains("final merge tree is unsafe"));
}

#[test]
fn finish_uses_the_supplied_integration_commit_message() {
    let env = Env::new();
    let (id, path) = env.create("finish-merge-message");
    commit_file(&path, "task.txt", "task\n");
    commit_file(&env.repo, "main.txt", "main\n");
    let output = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--integration-message",
        "chore: integrate main advance",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        git(&env.repo, &["log", "-1", "--format=%s", "main"]),
        "chore: integrate main advance"
    );
}

#[test]
fn finish_aborts_a_clean_merge_when_the_commit_message_hook_rejects_it() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-retry");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(
        &env,
        "case \"$(cat \"$1\")\" in *'chore: integrate task worktree'*) exit 1;; esac\nexit 0",
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("merge was aborted and task worktree restored"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
    assert!(!env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("task.txt")).unwrap(), b"task\n");
    assert!(!path.join("main.txt").exists());

    env.ok(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--integration-message",
        "chore: integrate prepared main update",
    ]);
    assert_eq!(git(&env.repo, &["show", "HEAD:task.txt"]), "task");
    assert_eq!(git(&env.repo, &["show", "HEAD:main.txt"]), "main");
    assert_eq!(
        git(&env.repo, &["log", "-1", "--format=%s", "main"]),
        "chore: integrate prepared main update"
    );
}

#[test]
fn finish_aborts_a_rejected_merge_with_annex_filter_backed_files() {
    let env = Env::new();
    let annexed = "media/annex-filtered.jpg";
    let annex_blob = add_annex_filter_backed_file(&env, annexed);
    let (id, path) = env.create_annex("finish-annex-filter-recovery");
    let task_tip = commit_file(&path, "task.txt", "task work\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main work\n");
    let raw = git(&path, &["hash-object", "--no-filters", "--", annexed]);
    let filtered = git(
        &path,
        &[
            "hash-object",
            "--filters",
            &format!("--path={annexed}"),
            "--",
            annexed,
        ],
    );
    assert_ne!(raw, annex_blob);
    assert_eq!(filtered, annex_blob);
    write_commit_msg_hook(&env, "exit 1");

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--allow-annex-filter",
        "--integration-message",
        "chore: rejected annex merge",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("merge was aborted and task worktree restored"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(git(&env.repo, &["rev-parse", "main"]), main_tip);
    assert_eq!(git(&path, &["status", "--porcelain"]), "");
    assert!(!env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(
        git(
            &path,
            &[
                "hash-object",
                "--filters",
                &format!("--path={annexed}"),
                annexed
            ]
        ),
        annex_blob
    );
}

#[test]
fn integration_and_finish_refuse_main_worktree_annex_filter_override() {
    let env = Env::new();
    let annexed = "media/main-filtered.jpg";
    add_annex_filter_backed_file(&env, annexed);
    git(&env.repo, &["config", "extensions.worktreeConfig", "true"]);
    let (id, path) = env.create_annex("main-filter-override");
    fs::write(path.join(annexed), vec![b'y'; 8192]).unwrap();
    git(&path, &["add", "--", annexed]);
    git(&path, &["commit", "-q", "-m", "update annexed file"]);
    let task_tip = git(&path, &["rev-parse", "HEAD"]);
    let main_tip = env.main_oid();
    let marker = env.tmp.join("main-annex-smudge-ran");
    let overridden_smudge = format!("sh -c 'touch {}; cat'", marker.display());
    git(
        &env.repo,
        &[
            "config",
            "--worktree",
            "filter.annex.smudge",
            &overridden_smudge,
        ],
    );

    let repo_arg = env.repo_arg();
    for command in ["integrate", "finish"] {
        let mut args = vec![
            command,
            "--repo",
            &repo_arg,
            "--id",
            &id,
            "--external-writers-quiesced",
            "--allow-annex-filter",
        ];
        if command == "finish" {
            args.extend(["--integration-message", "docs: reject main filter override"]);
        }
        let refused = env.run(&args);
        assert_eq!(refused.status.code(), Some(4), "{command}: {refused:?}");
        assert!(String::from_utf8_lossy(&refused.stderr)
            .contains("git-annex filter configuration is not verified"));
        assert!(!marker.exists());
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
        assert_eq!(env.main_oid(), main_tip);
    }
}

#[test]
fn finish_preserves_failed_merge_when_hook_changes_annex_smudge_filter() {
    let env = Env::new();
    let annexed = "media/task-only.jpg";
    add_annex_filter_backed_file(&env, annexed);
    let (id, path) = env.create_annex("finish-annex-hook");
    let task_tip = commit_file(&path, "task.txt", "task work\n");
    git(&env.repo, &["rm", "-q", annexed]);
    let main_tip = commit_file(&env.repo, "main.txt", "main work\n");
    let marker = env.tmp.join("annex-smudge-ran");
    let hook = format!(
        "git config filter.annex.smudge \"sh -c 'touch {}; cat'\"\nexit 1",
        marker.display()
    );
    write_commit_msg_hook(&env, &hook);

    let failed = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--allow-annex-filter",
        "--integration-message",
        "docs: reject merge after filter override",
    ]);
    assert_eq!(failed.status.code(), Some(3), "{failed:?}");
    assert!(String::from_utf8_lossy(&failed.stderr)
        .contains("task worktree preserved; automatic recovery was not completed"));
    assert!(String::from_utf8_lossy(&failed.stderr)
        .contains("git-annex filter configuration is not verified"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(env.main_oid(), main_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert!(!path.join(annexed).exists());
    assert!(!marker.exists());
}

#[test]
fn finish_recovers_a_verified_failed_merge_only_with_opt_in() {
    let env = Env::new();
    let annexed = "-recovery-filtered.jpg";
    let annex_blob = add_annex_filter_backed_file(&env, annexed);
    git(&env.repo, &["config", "extensions.worktreeConfig", "true"]);
    let (id, path) = env.create_annex("finish-delayed-recovery");
    let task_tip = commit_file(&path, "task.txt", "task work\n");
    let first_main_tip = commit_file(&env.repo, "main.txt", "main work\n");
    assert_ne!(
        git(&path, &["hash-object", "--no-filters", "--", annexed]),
        annex_blob
    );
    assert_eq!(
        git(
            &path,
            &[
                "hash-object",
                "--filters",
                &format!("--path={annexed}"),
                "--",
                annexed,
            ]
        ),
        annex_blob
    );
    env.exclude("/.direnv/");
    write_stale_nix_direnv_cache(&path);
    let preserved = path.join(".direnv/bin/nix-direnv-reload");
    let clean_marker = env.tmp.join("task-clean-filter-ran");
    let smudge_marker = env.tmp.join("task-smudge-filter-ran");
    let process_marker = env.tmp.join("task-process-filter-ran");
    let overridden_clean = format!("sh -c 'touch {}; cat'", clean_marker.display());
    let overridden_smudge = format!("sh -c 'touch {}; cat'", smudge_marker.display());
    let overridden_process = format!("sh -c 'touch {}; exit 1'", process_marker.display());
    git(
        &path,
        &[
            "config",
            "--worktree",
            "filter.annex.clean",
            &overridden_clean,
        ],
    );
    git(
        &path,
        &[
            "config",
            "--worktree",
            "filter.annex.smudge",
            &overridden_smudge,
        ],
    );
    git(
        &path,
        &[
            "config",
            "--worktree",
            "filter.annex.process",
            &overridden_process,
        ],
    );
    let process_setting = git(
        &path,
        &["config", "--show-origin", "--get", "filter.annex.process"],
    );
    assert!(
        process_setting.contains(&overridden_process),
        "task worktree filter override is not effective: {process_setting}"
    );
    assert!(!clean_marker.exists());
    assert!(!smudge_marker.exists());
    assert!(!process_marker.exists());
    let bad_task_config = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-annex-filter",
        "--allow-repository-hooks",
        "--integration-message",
        "docs: reject task filter override",
    ]);
    assert_eq!(
        bad_task_config.status.code(),
        Some(4),
        "{bad_task_config:?}"
    );
    assert!(String::from_utf8_lossy(&bad_task_config.stderr)
        .contains("git-annex filter configuration is not verified"));
    assert!(!clean_marker.exists());
    assert!(!smudge_marker.exists());
    assert!(!process_marker.exists());
    assert!(!env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(git(&env.repo, &["rev-parse", "main"]), first_main_tip);
    git(
        &path,
        &["config", "--worktree", "--unset", "filter.annex.clean"],
    );
    git(
        &path,
        &["config", "--worktree", "--unset", "filter.annex.smudge"],
    );
    git(
        &path,
        &["config", "--worktree", "--unset", "filter.annex.process"],
    );

    write_commit_msg_hook(
        &env,
        "printf 'hook\\n' >> .direnv/bin/nix-direnv-reload\nexit 1",
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--allow-annex-filter",
        "--integration-message",
        "docs: rejected first merge",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("worktree artifacts changed"));
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(fs::read(&preserved).unwrap(), b"reloadhook\n");

    let no_opt_in = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--allow-annex-filter",
        "--integration-message",
        "docs: refused without opt in",
    ]);
    assert_eq!(no_opt_in.status.code(), Some(4), "{no_opt_in:?}");
    assert!(String::from_utf8_lossy(&no_opt_in.stderr).contains("MERGE_HEAD in progress"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(env.main_oid(), first_main_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(fs::read(&preserved).unwrap(), b"reloadhook\n");

    git(
        &path,
        &[
            "config",
            "--worktree",
            "filter.annex.clean",
            &overridden_clean,
        ],
    );
    git(
        &path,
        &[
            "config",
            "--worktree",
            "filter.annex.smudge",
            &overridden_smudge,
        ],
    );
    git(
        &path,
        &[
            "config",
            "--worktree",
            "filter.annex.process",
            &overridden_process,
        ],
    );
    let recovery_process_setting = git(
        &path,
        &["config", "--show-origin", "--get", "filter.annex.process"],
    );
    assert!(
        recovery_process_setting.contains(&overridden_process),
        "task worktree filter override is not effective: {recovery_process_setting}"
    );
    assert!(!clean_marker.exists());
    assert!(!smudge_marker.exists());
    assert!(!process_marker.exists());
    let bad_recovery_config = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--allow-annex-filter",
        "--integration-message",
        "docs: reject task filter recovery override",
        "--keep-worktree",
    ]);
    assert_eq!(
        bad_recovery_config.status.code(),
        Some(4),
        "{bad_recovery_config:?}"
    );
    assert!(String::from_utf8_lossy(&bad_recovery_config.stderr)
        .contains("git-annex filter configuration is not verified"));
    assert!(
        !clean_marker.exists() && !smudge_marker.exists() && !process_marker.exists(),
        "filter ran during delayed recovery: {}; {}; {}; stderr: {}",
        clean_marker.exists(),
        smudge_marker.exists(),
        process_marker.exists(),
        String::from_utf8_lossy(&bad_recovery_config.stderr)
    );
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(fs::read(&preserved).unwrap(), b"reloadhook\n");
    git(
        &path,
        &["config", "--worktree", "--unset", "filter.annex.clean"],
    );
    git(
        &path,
        &["config", "--worktree", "--unset", "filter.annex.smudge"],
    );
    git(
        &path,
        &["config", "--worktree", "--unset", "filter.annex.process"],
    );

    fs::remove_file(env.repo.join(".git/hooks/commit-msg")).unwrap();
    fs::write(&preserved, "reload").unwrap();
    fs::set_permissions(&preserved, fs::Permissions::from_mode(0o755)).unwrap();
    let latest_main_tip = commit_file(&env.repo, "later-main.txt", "later main work\n");
    let recovered = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--allow-annex-filter",
        "--allow-repository-hooks",
        "--integration-message",
        "docs: recover and integrate prepared merge",
        "--keep-worktree",
    ]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(env.main_oid(), git(&path, &["rev-parse", "HEAD"]));
    assert_eq!(git(&env.repo, &["show", "HEAD:task.txt"]), "task work");
    assert_eq!(git(&env.repo, &["show", "HEAD:main.txt"]), "main work");
    assert_eq!(
        git(&env.repo, &["show", "HEAD:later-main.txt"]),
        "later main work"
    );
    assert_eq!(
        git(&env.repo, &["log", "-1", "--format=%s", "main"]),
        "docs: recover and integrate prepared merge"
    );
    git(
        &env.repo,
        &["merge-base", "--is-ancestor", &latest_main_tip, "HEAD"],
    );
    assert!(!env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(fs::read(&preserved).unwrap(), b"reload");
    assert_eq!(
        git(
            &path,
            &[
                "hash-object",
                "--filters",
                &format!("--path={annexed}"),
                "--",
                annexed,
            ]
        ),
        annex_blob
    );
    assert!(git(&path, &["status", "--porcelain", "--ignored"]).contains("!! .direnv/"));
}

#[test]
fn finish_recovery_refuses_ignored_file_colliding_with_a_restored_path() {
    let env = Env::new();
    commit_file(&env.repo, "restore-on-abort.txt", "base content\n");
    let (id, path) = env.create("finish-recovery-path-collision");
    let task_tip = commit_file(&path, "task.txt", "task work\n");
    fs::remove_file(env.repo.join("restore-on-abort.txt")).unwrap();
    git(&env.repo, &["add", "--update"]);
    let main_tip = commit_file(&env.repo, "main.txt", "main work\n");
    fs::write(env.repo.join(".git/info/exclude"), "restore-on-abort.txt\n").unwrap();
    write_commit_msg_hook(&env, "printf 'user data\\n' > restore-on-abort.txt\nexit 1");

    let failed = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--integration-message",
        "docs: create pending merge",
    ]);
    assert_eq!(failed.status.code(), Some(3), "{failed:?}");
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    let user_file = path.join("restore-on-abort.txt");
    assert_eq!(fs::read(&user_file).unwrap(), b"user data\n");

    fs::remove_file(env.repo.join(".git/hooks/commit-msg")).unwrap();
    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--integration-message",
        "docs: must preserve restore collision",
        "--keep-worktree",
    ]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("collides with path"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(env.main_oid(), main_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(fs::read(&user_file).unwrap(), b"user data\n");
    assert!(git(&path, &["status", "--porcelain", "--ignored"]).contains("!! restore-on-abort.txt"));
}

#[test]
fn finish_recovery_refuses_custom_filter_on_task_only_restore_path() {
    let env = Env::new();
    fs::write(
        env.repo.join(".gitattributes"),
        "restore-custom.txt filter=custom\n",
    )
    .unwrap();
    git(&env.repo, &["config", "filter.custom.clean", "cat"]);
    git(&env.repo, &["config", "filter.custom.smudge", "cat"]);
    fs::write(env.repo.join("restore-custom.txt"), "task tree content\n").unwrap();
    git(&env.repo, &["add", ".gitattributes", "restore-custom.txt"]);
    git(
        &env.repo,
        &["commit", "-q", "-m", "add filtered restore path"],
    );
    let (id, path) = env.create("finish-custom-restore");
    let task_tip = commit_file(&path, "task.txt", "task work\n");
    fs::remove_file(env.repo.join("restore-custom.txt")).unwrap();
    git(&env.repo, &["add", "--update"]);
    let main_tip = commit_file(&env.repo, "main.txt", "main work\n");

    git(
        &path,
        &[
            "merge",
            "--no-commit",
            "--no-ff",
            "--no-overwrite-ignore",
            "-m",
            "prepare merge before filter override",
            "main",
        ],
    );
    let marker = env.tmp.join("custom-smudge-ran");
    let smudge = format!("sh -c 'touch {}; cat'", marker.display());
    git(&path, &["config", "filter.custom.smudge", &smudge]);

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--integration-message",
        "docs: reject custom restore filter",
        "--keep-worktree",
    ]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("task-tip paths restored by abort use an unreviewed or mismatched filter"));
    assert!(!marker.exists());
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(env.main_oid(), main_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
}

#[test]
fn finish_recovery_refuses_merge_autostash_without_mutating() {
    let env = Env::new();
    let (id, path) = env.create("finish-recovery-autostash");
    let task_tip = commit_file(&path, "task.txt", "task work\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main work\n");
    env.exclude("sentinel.txt");
    write_commit_msg_hook(&env, "printf 'preserved\\n' > sentinel.txt\nexit 1");

    let failed = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--integration-message",
        "docs: leave failed merge",
    ]);
    assert_eq!(failed.status.code(), Some(3), "{failed:?}");
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    fs::remove_file(env.repo.join(".git/hooks/commit-msg")).unwrap();

    fs::write(env.admin(&id).join("ORIG_HEAD"), format!("{main_tip}\n")).unwrap();
    let wrong_orig = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--integration-message",
        "docs: refuse wrong ORIG_HEAD",
        "--keep-worktree",
    ]);
    assert_eq!(wrong_orig.status.code(), Some(4), "{wrong_orig:?}");
    assert!(String::from_utf8_lossy(&wrong_orig.stderr).contains("ORIG_HEAD does not match"));
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(fs::read(path.join("sentinel.txt")).unwrap(), b"preserved\n");
    fs::write(env.admin(&id).join("ORIG_HEAD"), format!("{task_tip}\n")).unwrap();

    fs::write(env.admin(&id).join("MERGE_HEAD"), format!("{task_tip}\n")).unwrap();
    let wrong_merge = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--integration-message",
        "docs: refuse wrong MERGE_HEAD",
        "--keep-worktree",
    ]);
    assert_eq!(wrong_merge.status.code(), Some(4), "{wrong_merge:?}");
    assert!(
        String::from_utf8_lossy(&wrong_merge.stderr).contains("not an ancestor of current main")
    );
    fs::write(env.admin(&id).join("MERGE_HEAD"), format!("{main_tip}\n")).unwrap();

    let task_file = path.join("task.txt");
    fs::remove_file(&task_file).unwrap();
    std::os::unix::fs::symlink("different-target", &task_file).unwrap();
    let wrong_type = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--integration-message",
        "docs: refuse changed worktree type",
        "--keep-worktree",
    ]);
    assert_eq!(wrong_type.status.code(), Some(4), "{wrong_type:?}");
    assert!(String::from_utf8_lossy(&wrong_type.stderr)
        .contains("tracked worktree files differ from the merge index"));
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    fs::remove_file(&task_file).unwrap();
    fs::write(&task_file, "task work\n").unwrap();

    fs::write(
        env.admin(&id).join("CHERRY_PICK_HEAD"),
        b"another operation\n",
    )
    .unwrap();

    let other_operation = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--integration-message",
        "docs: refuse another operation",
        "--keep-worktree",
    ]);
    assert_eq!(
        other_operation.status.code(),
        Some(4),
        "{other_operation:?}"
    );
    assert!(String::from_utf8_lossy(&other_operation.stderr)
        .contains("another Git operation is in progress (CHERRY_PICK_HEAD)"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(env.main_oid(), main_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    fs::remove_file(env.admin(&id).join("CHERRY_PICK_HEAD")).unwrap();
    fs::write(env.admin(&id).join("MERGE_AUTOSTASH"), b"autostash\n").unwrap();

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
        "--integration-message",
        "docs: do not apply autostash",
        "--keep-worktree",
    ]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("MERGE_AUTOSTASH is present"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(env.main_oid(), main_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(fs::read(path.join("sentinel.txt")).unwrap(), b"preserved\n");
}

#[test]
fn finish_recover_failed_merge_requires_an_explicit_integration_message() {
    let env = Env::new();
    let (id, path) = env.create("finish-recovery-message-required");
    let main_tip = env.main_oid();
    let output = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--recover-failed-merge",
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("requires an explicit --integration-message"));
    assert_eq!(env.main_oid(), main_tip);
    assert!(path.is_dir());
}

#[test]
fn finish_preserves_an_unexpected_index_change_after_a_failed_merge_commit() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-index");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(
        &env,
        "printf 'hook\\n' > hook-added.txt\ngit add hook-added.txt\nexit 1",
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("merge index differs from the preflight tree"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("hook-added.txt")).unwrap(), b"hook\n");
    assert!(git(&path, &["diff", "--cached", "--name-only"]).contains("hook-added.txt"));
}

#[test]
fn finish_preserves_a_tracked_worktree_change_after_a_failed_merge_commit() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-worktree");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(&env, "printf 'hook\\n' >> task.txt\nexit 1");

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("tracked worktree files differ from the merge index"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("task.txt")).unwrap(), b"task\nhook\n");
}

#[test]
fn finish_detects_same_size_worktree_edits_with_restored_mtime() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-stat-cache");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    let original = env.tmp.join("original-task.txt");
    git(&env.repo, &["config", "core.trustctime", "false"]);
    write_commit_msg_hook(
        &env,
        &format!(
            "cp task.txt '{}'\nprintf 'hask\\n' > task.txt\ntouch -r '{}' task.txt\nexit 1",
            original.display(),
            original.display(),
        ),
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("tracked worktree files differ from the merge index"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("task.txt")).unwrap(), b"hask\n");
}

#[test]
fn finish_preserves_raw_line_ending_changes_hidden_by_clean_conversion() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-line-endings");
    git(&env.repo, &["config", "core.autocrlf", "true"]);
    commit_file(&path, ".gitattributes", "*.txt text eol=lf\n");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(&env, "printf 'task\\r\\n' > task.txt\nexit 1");

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("tracked worktree files differ from the merge index"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("task.txt")).unwrap(), b"task\r\n");
}

#[test]
fn finish_reports_merge_abort_failure_without_a_destructive_fallback() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-abort");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(&env, "exit 1");
    let path_var = git_shim(
        &env,
        "shim-merge-abort",
        "if [ \"$3\" = merge ] && [ \"$4\" = --abort ]; then exit 1; fi",
    );

    let refused = env
        .cmd(&[
            "finish",
            "--repo",
            &env.repo_arg(),
            "--id",
            &id,
            "--external-writers-quiesced",
            "--allow-repository-hooks",
        ])
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("git merge --abort failed"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("current HEAD="));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("MERGE_HEAD="));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(git(&path, &["status", "--porcelain"]), "A  main.txt");
}

#[test]
fn finish_preserves_failed_merge_when_an_external_process_holds_a_worktree_file() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-open-file");
    commit_file(&path, "task.txt", "task\n");
    symlink("task.txt", path.join("task-link.txt")).unwrap();
    git(&path, &["add", "--", "task-link.txt"]);
    git(&path, &["commit", "-q", "-m", "add tracked symlink"]);
    let task_tip = git(&path, &["rev-parse", "HEAD"]);
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    let task_file = path.join("task.txt");
    let task_link = path.join("task-link.txt");
    write_commit_msg_hook(&env, "exit 1");
    let open_file = fs::OpenOptions::new()
        .append(true)
        .open(&task_link)
        .unwrap();

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    drop(open_file);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("processes have open handles in the task worktree"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(task_file).unwrap(), b"task\n");
}

#[test]
fn finish_preserves_a_hook_changed_orig_head_without_aborting() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-orig-head");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(
        &env,
        &format!("git update-ref ORIG_HEAD {main_tip}\nexit 1"),
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("ORIG_HEAD does not match"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(git(&path, &["rev-parse", "ORIG_HEAD"]), main_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(git(&path, &["status", "--porcelain"]), "A  main.txt");
}

#[test]
fn finish_preserves_an_unexpected_additional_merge_head_without_aborting() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-merge-head");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    let expected_merge_head_path =
        PathBuf::from(git(&path, &["rev-parse", "--git-path", "MERGE_HEAD"]));
    let expected_merge_head_path = if expected_merge_head_path.is_absolute() {
        expected_merge_head_path
    } else {
        path.join(expected_merge_head_path)
    };
    write_commit_msg_hook(&env, "exit 1");
    let path_var = git_shim_after(
        &env,
        "shim-extra-merge-head",
        &format!(
            "if [ \"$3\" = merge ] && [ \"$4\" != --abort ] && [ -f '{}' ]; then printf '{task_tip}\\n' >> '{}'; fi",
            expected_merge_head_path.display(),
            expected_merge_head_path.display(),
        ),
    );

    let refused = env
        .cmd(&[
            "finish",
            "--repo",
            &env.repo_arg(),
            "--id",
            &id,
            "--external-writers-quiesced",
            "--allow-repository-hooks",
        ])
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("MERGE_HEAD is missing or does not contain only the expected main commit"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(
        fs::read(env.admin(&id).join("MERGE_HEAD")).unwrap(),
        format!("{main_tip}\n{task_tip}\n").as_bytes()
    );
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("task.txt")).unwrap(), b"task\n");
}

#[test]
fn finish_preserves_a_hook_changed_merge_autostash_without_aborting() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-autostash");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(
        &env,
        &format!("git update-ref MERGE_AUTOSTASH {main_tip}\nexit 1"),
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("MERGE_AUTOSTASH is present"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert_eq!(
        git(&path, &["rev-parse", "--verify", "MERGE_AUTOSTASH"]),
        main_tip
    );
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("task.txt")).unwrap(), b"task\n");
}

#[test]
fn finish_refuses_to_start_with_a_merge_autostash_file() {
    let env = Env::new();
    let (id, path) = env.create("finish-existing-autostash");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    let autostash = PathBuf::from(git(&path, &["rev-parse", "--git-path", "MERGE_AUTOSTASH"]));
    let autostash = if autostash.is_absolute() {
        autostash
    } else {
        path.join(autostash)
    };
    fs::write(&autostash, format!("{main_tip}\n")).unwrap();

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
    ]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("MERGE_AUTOSTASH is present"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(!env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(
        fs::read(&autostash).unwrap(),
        format!("{main_tip}\n").as_bytes()
    );
}

#[test]
fn finish_preserves_index_flags_and_tracked_content_after_failed_merge_commit() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-index-flags");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(
        &env,
        "git update-index --assume-unchanged task.txt\nprintf 'hook\\n' >> task.txt\nexit 1",
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("assume-unchanged or skip-worktree"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(git(&path, &["ls-files", "-v", "--", "task.txt"]).starts_with("h "));
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(path.join("task.txt")).unwrap(), b"task\nhook\n");
}

#[test]
fn finish_detects_gitlink_index_changes_with_submodules_ignored() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-gitlink");
    commit_file(&path, "task.txt", "task\n");
    let original_gitlink = git(&path, &["rev-parse", "HEAD"]);
    git(
        &path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{original_gitlink},submodule"),
        ],
    );
    git(&path, &["commit", "-q", "-m", "add gitlink"]);
    let task_tip = git(&path, &["rev-parse", "HEAD"]);
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    git(&env.repo, &["config", "diff.ignoreSubmodules", "all"]);
    write_commit_msg_hook(
        &env,
        &format!("git update-index --cacheinfo 160000,{main_tip},submodule\nexit 1"),
    );

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("merge index differs from the preflight tree"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert!(git(&path, &["ls-files", "--stage", "--", "submodule"]).contains(&main_tip));
}

#[test]
fn finish_preserves_merge_state_for_worktrees_with_gitlinks() {
    let env = Env::new();
    let (id, path) = env.create("finish-existing-gitlink");
    commit_file(&path, "task.txt", "task\n");
    let gitlink = git(&path, &["rev-parse", "HEAD"]);
    git(
        &path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{gitlink},submodule"),
        ],
    );
    git(&path, &["commit", "-q", "-m", "add gitlink"]);
    let task_tip = git(&path, &["rev-parse", "HEAD"]);
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    git(&env.repo, &["config", "diff.ignoreSubmodules", "all"]);
    write_commit_msg_hook(&env, "exit 1");

    let refused = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("automatic recovery is disabled for worktrees containing gitlinks"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
}

#[test]
fn finish_reports_a_failed_abort_postcondition_without_overwriting_it() {
    let env = Env::new();
    let (id, path) = env.create("finish-hook-postcheck");
    let task_tip = commit_file(&path, "task.txt", "task\n");
    let main_tip = commit_file(&env.repo, "main.txt", "main\n");
    write_commit_msg_hook(&env, "exit 1");
    let task_file = path.join("task.txt");
    let after_abort = format!(
        "if [ \"$3\" = merge ] && [ \"$4\" = --abort ]; then printf 'hook\\n' >> '{}'; fi",
        task_file.display()
    );
    let path_var = git_shim_after(&env, "shim-post-abort-change", &after_abort);

    let refused = env
        .cmd(&[
            "finish",
            "--repo",
            &env.repo_arg(),
            "--id",
            &id,
            "--external-writers-quiesced",
            "--allow-repository-hooks",
        ])
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr)
        .contains("abort completed but tracked worktree files differ from the starting tree"));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), task_tip);
    assert!(!env.admin(&id).join("MERGE_HEAD").exists());
    assert_eq!(env.main_oid(), main_tip);
    assert_eq!(fs::read(task_file).unwrap(), b"task\nhook\n");
}

#[test]
fn finish_refuses_an_unreviewed_filter_or_unapproved_commit_signer() {
    for (name, setting, value, expected) in [
        (
            "filter",
            "filter.unreviewed.clean",
            "command:unreviewed",
            "unreviewed or mismatched filter",
        ),
        (
            "signer",
            "commit.gpgsign",
            "true",
            "authorize it with --allow-commit-signing",
        ),
    ] {
        let env = Env::new();
        let (id, path) = env.create(&format!("finish-{name}"));
        commit_file(&path, "task.txt", "task\n");
        commit_file(&env.repo, "main.txt", "main\n");
        if name == "filter" {
            fs::write(path.join(".gitattributes"), "*.txt filter=unreviewed\n").unwrap();
            git(&path, &["add", ".gitattributes"]);
            git(&path, &["commit", "-q", "-m", "add filter attribute"]);
        }
        git(&env.repo, &["config", setting, value]);
        let main = env.main_oid();
        let output = env.run(&[
            "finish",
            "--repo",
            &env.repo_arg(),
            "--id",
            &id,
            "--external-writers-quiesced",
        ]);
        assert_eq!(output.status.code(), Some(4), "{name}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{name}: {output:?}"
        );
        assert_eq!(env.main_oid(), main);
        assert!(path.is_dir());
    }
}

#[test]
fn finish_preserves_the_task_worktree_when_an_advanced_main_conflicts() {
    let env = Env::new();
    let (id, path) = env.create("finish-conflict");
    commit_file(&path, "f.txt", "task side\n");
    let main_tip = commit_file(&env.repo, "f.txt", "main side\n");
    let out = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
    ]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("worktree preserved"));
    assert_eq!(env.main_oid(), main_tip);
    assert!(path.exists());
    assert!(!env.admin(&id).join("MERGE_HEAD").exists());
}

#[test]
fn git_annex_worktree_is_restage_checked_and_removed_from_a_symlink_gitdir() {
    let env = Env::new();
    git(&env.repo, &["annex", "init", "-q"]);
    fs::write(env.repo.join("annexed.bin"), vec![b'x'; 8192]).unwrap();
    git(&env.repo, &["annex", "add", "--force-large", "annexed.bin"]);
    git(&env.repo, &["commit", "-q", "-m", "annexed"]);

    let missing_ack = env.run(&[
        "create",
        "--repo",
        &env.repo_arg(),
        "--slug",
        "annex-noack",
        "--task",
        "annex worktree",
        "--allow-repository-hooks",
    ]);
    assert_eq!(missing_ack.status.code(), Some(4));

    let task: Value = serde_json::from_str(&env.ok(&[
        "create",
        "--repo",
        &env.repo_arg(),
        "--slug",
        "annex-task",
        "--task",
        "annex worktree",
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--json",
    ]))
    .unwrap();
    let id = task["task_id"].as_str().unwrap();
    let path = PathBuf::from(task["worktree"].as_str().unwrap());
    git(&path, &["annex", "restage"]);
    assert!(
        fs::symlink_metadata(path.join(".git"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "git-annex restage should exercise the symlinked .git form"
    );
    let dry_run = env.ok(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        id,
        "--external-writers-quiesced",
        "--dry-run",
    ]);
    assert!(dry_run.contains("dry-run ok"), "{dry_run}");
    assert!(fs::symlink_metadata(path.join(".git"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(path.exists());
    let tip = commit_file(&path, "task.txt", "task work\n");
    assert!(fs::symlink_metadata(path.join(".git"))
        .unwrap()
        .file_type()
        .is_symlink());
    let main_tip = commit_file(&env.repo, "main.txt", "main work\n");

    let missing_remove_ack = env
        .raw_cmd(&["remove", "--repo", &env.repo_arg(), "--id", id])
        .output()
        .unwrap();
    assert_eq!(missing_remove_ack.status.code(), Some(4));
    assert!(path.exists());

    let missing_filter_ack = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
    ]);
    assert_eq!(missing_filter_ack.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&missing_filter_ack.stderr).contains("--allow-annex-filter"));
    assert!(path.exists());

    let out = env.ok(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--allow-annex-filter",
    ]);
    assert!(out.contains(&format!("finished {id}")), "{out}");
    let finish_tip = env.main_oid();
    assert_ne!(finish_tip, tip);
    git(
        &env.repo,
        &["merge-base", "--is-ancestor", &tip, &finish_tip],
    );
    git(
        &env.repo,
        &["merge-base", "--is-ancestor", &main_tip, &finish_tip],
    );
    assert_eq!(git(&env.repo, &["show", "HEAD:task.txt"]), "task work");
    assert_eq!(git(&env.repo, &["show", "HEAD:main.txt"]), "main work");
    assert!(!path.exists());
    assert!(!git(&env.repo, &["worktree", "list", "--porcelain"]).contains(path.to_str().unwrap()));
}

#[test]
fn git_annex_finish_quarantines_recognized_direnv_data() {
    let env = Env::new();
    git(&env.repo, &["annex", "init", "-q"]);
    fs::write(env.repo.join("annexed.bin"), vec![b'x'; 8192]).unwrap();
    git(&env.repo, &["annex", "add", "--force-large", "annexed.bin"]);
    git(&env.repo, &["commit", "-q", "-m", "annexed"]);

    let (id, path) = env.create_annex("annex-direnv");
    commit_file(&path, "task.txt", "task work\n");
    commit_file(&env.repo, "main.txt", "main work\n");
    env.exclude("/.direnv/");
    fs::create_dir_all(path.join(".direnv/user-data")).unwrap();
    fs::write(path.join(".direnv/user-data/keep"), b"direnv data").unwrap();

    let finished = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--allow-annex-filter",
    ]);
    assert!(
        finished.status.success(),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(!path.exists());
    let operation = env
        .quarantine_root()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::read(operation.join(".direnv/user-data/keep")).unwrap(),
        b"direnv data"
    );
}

#[test]
fn git_annex_finish_discards_stale_nix_direnv_cache() {
    let env = Env::new();
    git(&env.repo, &["annex", "init", "-q"]);
    fs::write(env.repo.join("annexed.bin"), vec![b'x'; 8192]).unwrap();
    git(&env.repo, &["annex", "add", "--force-large", "annexed.bin"]);
    git(&env.repo, &["commit", "-q", "-m", "annexed"]);

    let (id, path) = env.create_annex("annex-stale-direnv");
    commit_file(&path, "task.txt", "task work\n");
    commit_file(&env.repo, "main.txt", "main work\n");
    env.exclude("/.direnv/");
    let (input_target, profile_target) = write_stale_nix_direnv_cache(&path);

    let finished = env.run(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--external-writers-quiesced",
        "--allow-repository-hooks",
        "--allow-annex-filter",
    ]);
    assert!(
        finished.status.success(),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(!path.exists());
    assert!(env.quarantine_root().read_dir().unwrap().next().is_none());
    assert!(fs::symlink_metadata(input_target).is_err());
    assert!(fs::symlink_metadata(profile_target).is_err());
}

#[test]
fn git_annex_remove_refuses_unknown_ignored_data_before_restage() {
    let env = Env::new();
    git(&env.repo, &["annex", "init", "-q"]);
    fs::write(env.repo.join("annexed.bin"), vec![b'x'; 8192]).unwrap();
    git(&env.repo, &["annex", "add", "--force-large", "annexed.bin"]);
    git(&env.repo, &["commit", "-q", "-m", "annexed"]);

    let (id, path) = env.create_annex("annex-unknown-ignored");
    env.exclude("/private-data/");
    fs::create_dir_all(path.join("private-data")).unwrap();
    fs::write(path.join("private-data/keep"), b"user data").unwrap();

    let refused = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("worktree has unrecognized changes"),
        "{refused:?}"
    );
    assert_eq!(
        fs::read(path.join("private-data/keep")).unwrap(),
        b"user data"
    );
}

#[test]
fn git_annex_remove_refuses_artifact_changed_during_restage() {
    let env = Env::new();
    git(&env.repo, &["annex", "init", "-q"]);
    fs::write(env.repo.join("annexed.bin"), vec![b'x'; 8192]).unwrap();
    git(&env.repo, &["annex", "add", "--force-large", "annexed.bin"]);
    git(&env.repo, &["commit", "-q", "-m", "annexed"]);

    let (id, path) = env.create_annex("annex-artifact-change");
    env.exclude("/target/");
    fs::create_dir_all(path.join("target/debug")).unwrap();
    let artifact = path.join("target/debug/artifact");
    fs::write(&artifact, b"before restage").unwrap();
    let path_var = git_shim(
        &env,
        "mutate-artifact-during-annex-restage",
        "if [ \"$sub\" = annex ] && [ \"$verb\" = restage ]; then printf 'changed during restage' > \"$WTCTL_TEST_MUTATE\"; fi",
    );

    let refused = env
        .cmd(&[
            "remove",
            "--repo",
            &env.repo_arg(),
            "--id",
            &id,
            "--discard-generated-build-outputs",
        ])
        .env("PATH", path_var)
        .env("WTCTL_TEST_MUTATE", &artifact)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("cleanup artifact plan changed during git annex restage"),
        "{refused:?}"
    );
    assert!(path.exists());
    assert_eq!(fs::read(&artifact).unwrap(), b"changed during restage");
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
fn integrate_refuses_submodule_gitlink_changes() {
    let env = Env::new();
    let (id, path) = env.create("submodule-gitlink");
    let commit = commit_file(&path, "gitlink-object.txt", "object\n");
    git(
        &path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},nested"),
        ],
    );
    git(&path, &["commit", "-q", "-m", "add gitlink"]);
    let output = env.run(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("submodule gitlink changes"));
    assert!(path.exists());
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
fn direct_mutating_lifecycle_commands_require_external_writer_acknowledgement() {
    let env = Env::new();
    let (id, path) = env.create("writer-ack");
    commit_file(&path, "task.txt", "task\n");
    let integrate = env
        .raw_cmd(&["integrate", "--repo", &env.repo_arg(), "--id", &id])
        .output()
        .unwrap();
    assert_eq!(integrate.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&integrate.stderr).contains("external-writers-quiesced"));
    env.ok(&["integrate", "--repo", &env.repo_arg(), "--id", &id]);
    let remove = env
        .raw_cmd(&["remove", "--repo", &env.repo_arg(), "--id", &id])
        .output()
        .unwrap();
    assert_eq!(remove.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&remove.stderr).contains("external-writers-quiesced"));
    env.ok(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(!path.exists());
    let (orphan, orphan_path) = env.create("reap-ack");
    env.kill_owner(&orphan);
    let reap = env
        .raw_cmd(&["reap", "--repo", &env.repo_arg(), "--grace-secs", "0"])
        .output()
        .unwrap();
    assert_eq!(reap.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&reap.stderr).contains("external-writers-quiesced"));
    env.ok(&["reap", "--repo", &env.repo_arg(), "--grace-secs", "0"]);
    assert!(!orphan_path.exists());
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
    assert_eq!(row["owner_state"], "unknown");
    assert_eq!(row["audit_state"], "UNKNOWN");
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

#[test]
fn legacy_audit_marks_a_dead_process_active_when_its_session_recently_updated() {
    let env = Env::new();
    let session = "0e7f96a6-1234-4abc-8def-0123456789ab";
    let path = env.tmp.join("state/proj/worktrees/recent-session");
    git(
        &env.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    let base = env.main_oid();
    let admin = env.repo.join(".git/worktrees/recent-session");
    fs::write(
        admin.join("agent-task"),
        format!(
            "recent task client=claude session={session} base={base} pid={}@1\n",
            absent_pid()
        ),
    )
    .unwrap();
    let session_file = env
        .tmp
        .join("home/.claude/projects/project")
        .join(format!("{session}.jsonl"));
    fs::create_dir_all(session_file.parent().unwrap()).unwrap();
    fs::write(session_file, "recent activity").unwrap();

    let mut command = env.cmd(&["list", "--repo", &env.repo_arg(), "--all", "--json"]);
    command
        .env("CLAUDE_CODE_SESSION_ID", session)
        .env("CLAUDE_CONFIG_DIR", env.tmp.join("empty-claude-config"));
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let row = rows.iter().find(|r| r["id"] == "recent-session").unwrap();
    assert_eq!(row["owner_state"], "alive");
    assert_eq!(row["audit_state"], "ACTIVE");
    assert!(row["session_idle_hours"].as_f64().unwrap() < 2.0);
    assert_eq!(row["integrated"], true);
    assert_eq!(row["removable"], "legacy audit state is ACTIVE");
}

#[test]
fn legacy_audit_offers_only_stale_merged_worktrees_owned_by_the_current_session() {
    let env = Env::new();
    let session = "0e7f96a6-1234-4abc-8def-0123456789ab";
    let path = env.tmp.join("state/proj/worktrees/stale-merged");
    git(
        &env.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    let base = env.main_oid();
    let admin = env.repo.join(".git/worktrees/stale-merged");
    fs::write(
        admin.join("agent-task"),
        format!(
            "stale task client=claude session={session} base={base} pid={}@1\n",
            absent_pid()
        ),
    )
    .unwrap();
    let task_tip = commit_file(&path, "task.txt", "merged\n");
    git(&env.repo, &["merge", "--ff-only", &task_tip]);
    let session_file = env
        .tmp
        .join("home/.claude/projects/project")
        .join(format!("{session}.jsonl"));
    fs::create_dir_all(session_file.parent().unwrap()).unwrap();
    fs::write(&session_file, "old activity").unwrap();
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    fs::File::open(&session_file)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();

    let mut command = env.cmd(&[
        "list",
        "--repo",
        &env.repo_arg(),
        "--legacy",
        "--json",
        "--base",
        &task_tip,
    ]);
    command.env("CLAUDE_CODE_SESSION_ID", session);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let row = rows.iter().find(|r| r["id"] == "stale-merged").unwrap();
    assert_eq!(row["audit_state"], "IDLE-MERGED");
    assert_eq!(row["mine"], true);
    assert_eq!(row["removable"], true);
    let suggested: Vec<String> = serde_json::from_value(row["suggested_remove"].clone()).unwrap();
    assert_eq!(suggested[0], "wtctl");
    assert!(suggested.contains(&"--legacy-worktree-id".into()));
    assert!(suggested.contains(&"--base".into()));
    assert!(suggested.contains(&task_tip));
    assert!(suggested.contains(&"--dry-run".into()));
}

#[test]
fn legacy_branch_worktrees_are_visible_in_all_and_legacy_lists() {
    let env = Env::new();
    let branch = "legacy-branch";
    let path = env.tmp.join("legacy-branch-worktree");
    git(&env.repo, &["branch", branch, "HEAD"]);
    git(
        &env.repo,
        &["worktree", "add", "-q", path.to_str().unwrap(), branch],
    );
    let base = env.main_oid();
    git(
        &env.repo,
        &[
            "config",
            &format!("branch.{branch}.description"),
            &format!(
                "legacy branch client=claude session=0e7f96a6-1234-4abc-8def-0123456789ab base={base} pid={}@1",
                absent_pid()
            ),
        ],
    );
    let all = env.list("--all");
    let row = all.iter().find(|r| r["branch"] == branch).unwrap();
    assert_eq!(row["kind"], "legacy");
    assert_eq!(row["owner"], "claude:0e7f96a6-1234-4abc-8def-0123456789ab");
    assert_eq!(row["owner_state"], "unknown");
    assert!(env.list("--legacy").iter().any(|r| r["branch"] == branch));
    assert!(env.list("--orphans").is_empty());
}

#[test]
fn remove_accepts_an_explicit_commit_base_ref() {
    let env = Env::new();
    let (id, path) = env.create("remove-custom-base");
    let base = env.main_oid();
    let output = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--base",
        &base,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!path.exists());
}

#[test]
fn remove_legacy_detached_worktree_requires_the_exact_current_session() {
    let env = Env::new();
    let session = "0e7f96a6-1234-4abc-8def-0123456789ab";
    let path = env.tmp.join("state/proj/worktrees/old-detached");
    git(
        &env.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            path.to_str().unwrap(),
            "HEAD",
        ],
    );
    let id = "old-detached";
    let base = env.main_oid();
    fs::write(
        env.repo.join(".git/worktrees/old-detached/agent-task"),
        format!("old task client=claude session={session} base={base}\n"),
    )
    .unwrap();
    let mut wrong_session = env.cmd(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--legacy-worktree-id",
        id,
    ]);
    wrong_session.env(
        "CLAUDE_CODE_SESSION_ID",
        "9e7f96a6-1234-4abc-8def-0123456789ab",
    );
    assert_eq!(wrong_session.output().unwrap().status.code(), Some(4));
    assert!(path.exists());

    let mut current_session = env.cmd(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--legacy-worktree-id",
        id,
    ]);
    current_session.env("CLAUDE_CODE_SESSION_ID", session);
    let output = current_session.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!path.exists());
}

#[test]
fn remove_legacy_branch_worktree_can_delete_only_its_verified_branch() {
    let env = Env::new();
    let session = "0e7f96a6-1234-4abc-8def-0123456789ab";
    let branch = "legacy-branch";
    let path = env.tmp.join("legacy-branch-worktree");
    git(&env.repo, &["branch", branch, "HEAD"]);
    git(
        &env.repo,
        &["worktree", "add", "-q", path.to_str().unwrap(), branch],
    );
    let base = env.main_oid();
    git(
        &env.repo,
        &[
            "config",
            &format!("branch.{branch}.description"),
            &format!("legacy branch client=claude session={session} base={base}"),
        ],
    );
    let mut command = env.cmd(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--expect-branch",
        branch,
        "--delete-branch",
    ]);
    command.env("CLAUDE_CODE_SESSION_ID", session);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!path.exists());
    assert!(!git_cmd(
        &env.repo,
        &["show-ref", "--verify", &format!("refs/heads/{branch}")]
    )
    .output()
    .unwrap()
    .status
    .success());
}

#[test]
fn finish_legacy_branch_worktree_merges_advancing_main_and_removes_the_owned_branch() {
    let env = Env::new();
    let session = "0e7f96a6-1234-4abc-8def-0123456789ab";
    let branch = "legacy-finish";
    let path = env.tmp.join("legacy-finish-worktree");
    git(&env.repo, &["branch", branch, "HEAD"]);
    git(
        &env.repo,
        &["worktree", "add", "-q", path.to_str().unwrap(), branch],
    );
    let base = env.main_oid();
    git(
        &env.repo,
        &[
            "config",
            &format!("branch.{branch}.description"),
            &format!("legacy branch client=claude session={session} base={base}"),
        ],
    );
    let branch_tip = commit_file(&path, "legacy-task.txt", "legacy work\n");
    commit_file(&env.repo, "main.txt", "main work\n");

    let mut command = env.cmd(&[
        "finish",
        "--repo",
        &env.repo_arg(),
        "--expect-branch",
        branch,
        "--delete-branch",
        "--external-writers-quiesced",
    ]);
    command.env("CLAUDE_CODE_SESSION_ID", session);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let head = env.main_oid();
    assert!(git(
        &env.repo,
        &["merge-base", "--is-ancestor", &branch_tip, &head]
    )
    .is_empty());
    assert_eq!(
        git(&env.repo, &["show", "HEAD:legacy-task.txt"]),
        "legacy work"
    );
    assert_eq!(git(&env.repo, &["show", "HEAD:main.txt"]), "main work");
    assert!(!path.exists());
    assert!(!git_cmd(
        &env.repo,
        &["show-ref", "--verify", &format!("refs/heads/{branch}")]
    )
    .output()
    .unwrap()
    .status
    .success());
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

fn write_commit_msg_hook(env: &Env, script: &str) {
    let hooks = env.repo.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let path = hooks.join("commit-msg");
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
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
fn explicit_reflog_opt_in_allows_removal_when_head_is_integrated() {
    let env = Env::new();
    let (id, path) = env.create("allow-reflog");
    commit_file(&path, "lost.txt", "work\n");
    git(&path, &["reset", "-q", "--hard", "HEAD~1"]);
    let refused = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    let removed = env.run(&[
        "remove",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--allow-unreferenced-reflog",
    ]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!path.exists());
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
    let deadline = Instant::now() + Duration::from_secs(60);
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
    let real = real_git_executable().to_string_lossy().into_owned();
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
fn failed_removal_restores_a_verified_git_annex_pointer() {
    use std::os::unix::fs::symlink;

    let env = Env::new();
    let (id, path) = env.create("restore-annex-pointer");
    let admin = env.admin(&id);
    fs::remove_file(path.join(".git")).unwrap();
    symlink(&admin, path.join(".git")).unwrap();
    let real = real_git_executable().to_string_lossy().into_owned();
    let shim_dir = env.tmp.join("annex-restore-shim");
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
    assert_ne!(out.status.code(), Some(0));
    let pointer = fs::symlink_metadata(path.join(".git")).unwrap();
    assert!(pointer.file_type().is_symlink());
    assert_eq!(fs::canonicalize(path.join(".git")).unwrap(), admin);
    assert_eq!(
        fs::read_to_string(env.admin(&id).join("locked"))
            .unwrap()
            .trim(),
        format!("agent-task {id}")
    );
}

#[test]
fn failed_partial_removal_does_not_restore_over_a_changed_git_pointer() {
    use std::os::unix::fs::symlink;

    let env = Env::new();
    let (id, path) = env.create("partial-remove");
    let admin = env.admin(&id);
    fs::remove_file(path.join(".git")).unwrap();
    symlink(&admin, path.join(".git")).unwrap();
    let real = real_git_executable().to_string_lossy().into_owned();
    let shim_dir = env.tmp.join("partial-shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = worktree ] && [ \"$a\" = remove ]; then\n    rm \"$WTCTL_TEST_WT/.git\"\n    printf 'gitdir: invalid\\n' > \"$WTCTL_TEST_WT/.git\"\n    exit 1\n  fi\n  prev=$a\ndone\nexec '{real}' \"$@\"\n"
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
        .env("WTCTL_TEST_WT", &path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("rollback stopped safely"), "{stderr}");
    assert!(path.is_dir());
    assert_eq!(
        fs::read_to_string(path.join(".git")).unwrap(),
        "gitdir: invalid\n"
    );
    assert!(!admin.join("locked").exists());
}

#[test]
fn failed_removal_does_not_replace_a_lock_acquired_after_preflight() {
    let env = Env::new();
    let (id, path) = env.create("new-lock");
    let worktree = path.to_string_lossy().into_owned();
    git(&env.repo, &["worktree", "unlock", &worktree]);
    let real = real_git_executable().to_string_lossy().into_owned();
    let shim_dir = env.tmp.join("new-lock-shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = worktree ] && [ \"$a\" = remove ]; then\n    '{real}' -C \"$WTCTL_TEST_REPO\" worktree lock --reason 'foreign hold' \"$WTCTL_TEST_WT\"\n    exit 1\n  fi\n  prev=$a\ndone\nexec '{real}' \"$@\"\n"
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
        .env("WTCTL_TEST_REPO", &env.repo)
        .env("WTCTL_TEST_WT", &path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("worktree lock changed"), "{stderr}");
    assert_eq!(
        fs::read_to_string(env.admin(&id).join("locked"))
            .unwrap()
            .trim(),
        "foreign hold"
    );
    assert!(path.is_dir());
}

#[test]
fn failed_removal_does_not_recreate_a_worktree_after_registration_disappears() {
    let env = Env::new();
    let (id, path) = env.create("registration-gone");
    let worktree = path.to_string_lossy().into_owned();
    let real = real_git_executable().to_string_lossy().into_owned();
    let shim_dir = env.tmp.join("registration-gone-shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = worktree ] && [ \"$a\" = remove ]; then\n    '{real}' \"$@\"\n    exit 1\n  fi\n  prev=$a\ndone\nexec '{real}' \"$@\"\n"
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
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("registration disappeared"), "{stderr}");
    assert!(!path.exists());
    assert!(!git(&env.repo, &["worktree", "list", "--porcelain"]).contains(&worktree));
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
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_for(&mark);
    let lock = env.repo.join(".git/main-integration.lock");
    let owner = fs::read_to_string(lock.join("owner")).unwrap();
    let token = owner.lines().next().unwrap();
    assert_eq!(token.len(), 32);
    assert!(owner.lines().nth(1) == Some(&child.id().to_string()));
    assert_eq!(
        fs::create_dir(&lock).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
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
    let real = real_git_executable().to_string_lossy().into_owned();
    let dir = env.tmp.join(name);
    fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nsub=\nverb=\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = worktree ] || [ \"$prev\" = annex ]; then sub=$prev; verb=$a; fi\n  prev=$a\ndone\n{hook}\nexec '{real}' \"$@\"\n"
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

fn git_shim_after(env: &Env, name: &str, after: &str) -> String {
    let real = real_git_executable().to_string_lossy().into_owned();
    let dir = env.tmp.join(name);
    fs::create_dir_all(&dir).unwrap();
    let shim = dir.join("git");
    fs::write(
        &shim,
        format!("#!/bin/sh\n'{real}' \"$@\"\ncode=$?\n{after}\nexit \"$code\"\n"),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn real_git_executable() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find_map(|candidate| {
            let canonical = fs::canonicalize(candidate).ok()?;
            if cfg!(target_os = "macos") && canonical == Path::new("/usr/bin/git") {
                None
            } else {
                Some(canonical)
            }
        })
        .or_else(|| {
            if cfg!(target_os = "macos") {
                let output = Command::new("xcrun")
                    .args(["--find", "git"])
                    .output()
                    .ok()?;
                output
                    .status
                    .success()
                    .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
                    .and_then(|path| fs::canonicalize(path).ok())
            } else {
                None
            }
        })
        .expect("a Git executable that does not re-enter the PATH shim")
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
    let err = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(4), "{err}");
    assert!(err.contains(&within.id().to_string()), "{err}");
    if removed.status.success() {
        away.kill().unwrap();
        away.wait().unwrap();
    } else {
        let detail = String::from_utf8_lossy(&removed.stderr);
        assert!(detail.contains("may lie inside"), "{detail}");
        away.kill().unwrap();
        away.wait().unwrap();
        let retry = env.run(&["remove", "--repo", &env.repo_arg(), "--id", &id]);
        assert!(
            retry.status.success(),
            "{}",
            String::from_utf8_lossy(&retry.stderr)
        );
    }
    assert!(!path.exists());
}

#[test]
fn version_and_per_command_help_do_not_need_a_repository() {
    let env = Env::new();
    let version = env.ok(&["--version"]);
    assert_eq!(
        version.trim(),
        format!("wtctl {}", env!("CARGO_PKG_VERSION"))
    );
    for command in ["create", "finish", "remove", "list"] {
        let via_flag = env.ok(&[command, "--help"]);
        let via_help = env.ok(&["help", command]);
        assert_eq!(via_flag, via_help);
        assert!(via_flag.starts_with(&format!("usage: wtctl {command} ")));
    }
    assert!(env
        .ok(&["remove", "-h"])
        .contains("artifact cleanup options"));
    assert_eq!(env.run(&["help", "nope"]).status.code(), Some(2));
}

#[test]
fn explicit_session_is_recorded_shown_and_updatable() {
    let env = Env::new();
    let created: Value = serde_json::from_str(&env.ok(&[
        "create",
        "--repo",
        &env.repo_arg(),
        "--slug",
        "session-task",
        "--session",
        "ses_abc123",
        "--json",
    ]))
    .unwrap();
    let id = created["task_id"].as_str().unwrap().to_string();
    let path = created["worktree"].as_str().unwrap().to_string();
    assert_eq!(created["session_hint"], "ses_abc123");
    assert_eq!(
        env.run(&[
            "create",
            "--repo",
            &env.repo_arg(),
            "--slug",
            "bad-session",
            "--session",
            "has space",
        ])
        .status
        .code(),
        Some(2)
    );

    let whoami: Value =
        serde_json::from_str(&env.ok(&["whoami", "--path", &path, "--json"])).unwrap();
    assert_eq!(whoami["session"], "ses_abc123");
    // The test binary stands in for an agent named after itself, not a known client.
    assert!(whoami["resume_command"].is_null());
    let row = env
        .list("--all")
        .into_iter()
        .find(|r| r["id"] == id)
        .unwrap();
    assert_eq!(row["session"], "ses_abc123");

    env.ok(&[
        "describe",
        "--repo",
        &env.repo_arg(),
        "--id",
        &id,
        "--task",
        "renamed task",
        "--session",
        "ses_def456",
    ]);
    let marker = env.marker(&id);
    assert_eq!(marker["task"], "renamed task");
    assert_eq!(marker["session_hint"], "ses_def456");
    assert_eq!(
        marker["history"].as_array().unwrap().last().unwrap()["event"],
        "describe"
    );
    assert_eq!(
        env.run(&["describe", "--repo", &env.repo_arg(), "--id", &id])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn show_reports_task_details_and_unintegrated_commits() {
    let env = Env::new();
    let (id, path) = env.create("show-task");
    commit_file(&path, "show.txt", "shown\n");
    let report: Value =
        serde_json::from_str(&env.ok(&["show", "--repo", &env.repo_arg(), "--id", &id, "--json"]))
            .unwrap();
    assert_eq!(report["task_id"], id);
    assert_eq!(report["task"], "example task");
    assert_eq!(report["unintegrated"], 1);
    assert_eq!(report["commits"].as_array().unwrap().len(), 1);
    assert!(report["commits"][0].as_str().unwrap().ends_with("show.txt"));
    assert!(report["diffstat"]
        .as_str()
        .unwrap()
        .contains("1 file changed"));
    assert!(report["removable"].is_string());
    let text = env.ok(&["show", "--repo", &env.repo_arg(), "--id", &id]);
    assert!(text.contains(&format!("task_id={id}")));
    assert!(text.contains("commit "));
    assert!(text.contains("unintegrated=1"));
}

#[test]
fn abandon_preserves_unintegrated_work_and_restore_recovers_it() {
    let env = Env::new();
    let (id, path) = env.create("abandon-task");
    let tip = commit_file(&path, "keep.txt", "keep me\n");
    let repo = env.repo_arg();

    let plain = env.run(&["remove", "--repo", &repo, "--id", &id]);
    assert_eq!(plain.status.code(), Some(4));
    let flags_without_id = env.run(&[
        "remove",
        "--repo",
        &repo,
        "--abandon",
        "--expect-branch",
        "x",
    ]);
    assert_eq!(flags_without_id.status.code(), Some(2));

    env.ok(&[
        "remove",
        "--repo",
        &repo,
        "--id",
        &id,
        "--abandon",
        "--dry-run",
    ]);
    assert!(path.exists());
    let ref_name = format!("refs/wtctl/abandoned/{id}");
    let missing = git_cmd(&env.repo, &["rev-parse", "--verify", "--quiet", &ref_name])
        .output()
        .unwrap();
    assert!(!missing.status.success(), "dry-run must not create the ref");

    fs::write(path.join("dirty.txt"), "uncommitted\n").unwrap();
    let dirty = env.run(&["remove", "--repo", &repo, "--id", &id, "--abandon"]);
    assert_eq!(dirty.status.code(), Some(4));
    assert!(path.exists());
    fs::remove_file(path.join("dirty.txt")).unwrap();

    let removed: Value = serde_json::from_str(&env.ok(&[
        "remove",
        "--repo",
        &repo,
        "--id",
        &id,
        "--abandon",
        "--json",
    ]))
    .unwrap();
    assert_eq!(removed["status"], "removed");
    assert_eq!(removed["abandoned_ref"], ref_name);
    assert!(!path.exists());
    assert_eq!(git(&env.repo, &["rev-parse", &ref_name]), tip);
    assert_eq!(env.main_oid(), git(&env.repo, &["rev-parse", "main"]));

    let listed: Value =
        serde_json::from_str(&env.ok(&["list", "--repo", &repo, "--abandoned", "--json"])).unwrap();
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["head"], tip);
    assert_eq!(listed[0]["subject"], "keep.txt");

    let restored: Value = serde_json::from_str(&env.ok(&[
        "restore",
        "--repo",
        &repo,
        "--id",
        &id,
        "--slug",
        "restored-task",
        "--json",
    ]))
    .unwrap();
    let new_path = PathBuf::from(restored["worktree"].as_str().unwrap());
    assert_eq!(git(&new_path, &["rev-parse", "HEAD"]), tip);
    assert_eq!(restored["parent"], id);
    assert_eq!(
        env.run(&[
            "restore",
            "--repo",
            &repo,
            "--id",
            "no-such-task-20260101-00000000",
            "--slug",
            "no-such"
        ])
        .status
        .code(),
        Some(1)
    );
}

#[test]
fn finish_dry_run_reports_requirements_without_changing_anything() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("preflight-task");
    let tip = commit_file(&path, "pre.txt", "pre\n");
    let before = env.main_oid();

    let report: Value = serde_json::from_str(&env.ok(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--dry-run",
        "--json",
    ]))
    .unwrap();
    assert_eq!(report["status"], "ok");
    assert_eq!(report["integration"], "fast-forward");
    assert_eq!(report["new_commits"], 1);
    assert_eq!(report["task_tip"], tip);
    assert!(report["required_flags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|flag| flag == "--external-writers-quiesced"));
    assert_eq!(env.main_oid(), before);
    assert!(path.exists());

    // Diverge main so the real run needs a signed merge.
    commit_file(&env.repo, "main-only.txt", "main\n");
    git(&env.repo, &["config", "commit.gpgsign", "true"]);
    let needs = env.run(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--dry-run",
        "--json",
    ]);
    assert_eq!(needs.status.code(), Some(4));
    let report: Value = serde_json::from_slice(&needs.stdout).unwrap();
    assert_eq!(report["status"], "needs-flags");
    assert_eq!(report["integration"], "merge");
    assert!(report["required_flags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|flag| flag == "--allow-commit-signing"));
    assert!(!report.to_string().contains("\"error\""));
    git(&env.repo, &["config", "--unset", "commit.gpgsign"]);

    let ok = env.run(&["finish", "--repo", &repo, "--id", &id, "--dry-run"]);
    assert_eq!(ok.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("merge"));
    assert!(path.exists());
    assert_eq!(
        env.run(&[
            "finish",
            "--repo",
            &repo,
            "--expect-branch",
            "x",
            "--dry-run"
        ])
        .status
        .code(),
        Some(2)
    );
}

#[test]
fn finish_dry_run_reports_merge_conflicts_as_blocked() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("conflict-task");
    commit_file(&path, "f.txt", "task side\n");
    commit_file(&env.repo, "f.txt", "main side\n");
    let out = env.run(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--dry-run",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(4));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["status"], "blocked");
    let conflict = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "merge-conflicts")
        .unwrap();
    assert_eq!(conflict["state"], "blocked");
}

#[test]
fn finish_reports_the_main_movement_and_removed_path() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("report-task");
    let before = env.main_oid();
    let tip = commit_file(&path, "report.txt", "report\n");
    let text = env.ok(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--external-writers-quiesced",
    ]);
    assert!(text.contains(&format!("{before} -> {tip}")), "{text}");
    assert!(
        text.contains(&format!("removed {}", path.display())),
        "{text}"
    );
}

#[test]
fn finish_rebase_replays_task_commits_onto_an_advanced_main() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("rebase-task");
    let original = commit_file(&path, "task.txt", "task\n");
    let advanced = commit_file(&env.repo, "main.txt", "main\n");

    env.ok(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--rebase",
        "--external-writers-quiesced",
    ]);
    assert!(!path.exists());
    let tip = env.main_oid();
    assert_ne!(tip, original);
    assert_eq!(git(&env.repo, &["rev-parse", "HEAD~1"]), advanced);
    assert_eq!(git(&env.repo, &["show", "HEAD:task.txt"]), "task");
    assert_eq!(git(&env.repo, &["show", "HEAD:main.txt"]), "main");
    assert_eq!(
        git(
            &env.repo,
            &["rev-parse", &format!("refs/wtctl/superseded/{id}")]
        ),
        original
    );
}

#[test]
fn finish_rebase_conflict_leaves_the_worktree_and_the_original_tip() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("rebase-conflict");
    let original = commit_file(&path, "f.txt", "task side\n");
    commit_file(&env.repo, "f.txt", "main side\n");
    let main_before = env.main_oid();

    let out = env.run(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--rebase",
        "--external-writers-quiesced",
    ]);
    assert_eq!(out.status.code(), Some(4));
    assert!(path.exists());
    assert_eq!(env.main_oid(), main_before);
    assert_eq!(
        git(
            &env.repo,
            &["rev-parse", &format!("refs/wtctl/superseded/{id}")]
        ),
        original
    );
    assert!(env.admin(&id).join("rebase-merge").exists());
}

#[test]
fn finish_squash_integrates_one_commit_with_the_given_message() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("squash-task");
    commit_file(&path, "one.txt", "1\n");
    let last = commit_file(&path, "two.txt", "2\n");
    let before = env.main_oid();

    let missing = env.run(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--squash",
        "--external-writers-quiesced",
    ]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(path.exists());

    env.ok(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--squash",
        "--integration-message",
        "feat: squashed task",
        "--external-writers-quiesced",
    ]);
    assert!(!path.exists());
    assert_eq!(git(&env.repo, &["rev-parse", "HEAD~1"]), before);
    assert_eq!(
        git(&env.repo, &["log", "-1", "--format=%s"]),
        "feat: squashed task"
    );
    assert_eq!(git(&env.repo, &["show", "HEAD:one.txt"]), "1");
    assert_eq!(git(&env.repo, &["show", "HEAD:two.txt"]), "2");
    assert_eq!(
        git(
            &env.repo,
            &["rev-parse", &format!("refs/wtctl/superseded/{id}")]
        ),
        last
    );
    let both = env.run(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--squash",
        "--rebase",
        "--external-writers-quiesced",
    ]);
    assert_eq!(both.status.code(), Some(2));
}

#[test]
fn finish_dry_run_understands_rebase_and_squash() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("strategy-plan");
    commit_file(&path, "plan.txt", "plan\n");
    commit_file(&env.repo, "main.txt", "main\n");
    let rebase: Value = serde_json::from_str(&env.ok(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--rebase",
        "--dry-run",
        "--json",
    ]))
    .unwrap();
    assert_eq!(rebase["integration"], "rebase");
    assert_eq!(rebase["strategy"], "rebase");
    let squash: Value = serde_json::from_str(&env.ok(&[
        "finish",
        "--repo",
        &repo,
        "--id",
        &id,
        "--squash",
        "--integration-message",
        "feat: plan",
        "--dry-run",
        "--json",
    ]))
    .unwrap();
    assert_eq!(squash["strategy"], "squash");
    assert_eq!(
        env.run(&[
            "finish",
            "--repo",
            &repo,
            "--id",
            &id,
            "--squash",
            "--dry-run"
        ])
        .status
        .code(),
        Some(2)
    );
    assert!(path.exists());
}

#[test]
fn doctor_reports_problems_and_releases_only_a_provably_stale_lock() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("doctor-task");
    let clean: Value =
        serde_json::from_str(&env.ok(&["doctor", "--repo", &repo, "--json"])).unwrap();
    assert_eq!(clean["status"], "ok");

    let lock_dir = env.repo.join(".git/main-integration.lock");
    fs::create_dir(&lock_dir).unwrap();
    let host = String::from_utf8(Command::new("hostname").output().unwrap().stdout).unwrap();
    let host = host.trim();
    fs::write(
        lock_dir.join("owner"),
        format!("tok\n{}\n{host}\n", absent_pid()),
    )
    .unwrap();

    let stale = env.run(&["doctor", "--repo", &repo, "--json"]);
    assert_eq!(stale.status.code(), Some(4));
    let report: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(report["findings"][0]["code"], "lock-stale");
    assert!(lock_dir.exists());
    let unacked = env.run(&["doctor", "--repo", &repo, "--fix-stale-lock"]);
    assert_eq!(unacked.status.code(), Some(4));
    assert!(lock_dir.exists());

    let fixed: Value = serde_json::from_str(&env.ok(&[
        "doctor",
        "--repo",
        &repo,
        "--fix-stale-lock",
        "--external-writers-quiesced",
        "--json",
    ]))
    .unwrap();
    assert_eq!(fixed["status"], "ok");
    assert!(!lock_dir.exists());

    // A lock held by a live process, or from another host, is never touched.
    fs::create_dir(&lock_dir).unwrap();
    fs::write(
        lock_dir.join("owner"),
        format!("tok\n{}\n{host}\n", std::process::id()),
    )
    .unwrap();
    env.run(&[
        "doctor",
        "--repo",
        &repo,
        "--fix-stale-lock",
        "--external-writers-quiesced",
    ]);
    assert!(lock_dir.exists());
    fs::write(
        lock_dir.join("owner"),
        format!("tok\n{}\nother-host\n", absent_pid()),
    )
    .unwrap();
    let foreign: Value = serde_json::from_slice(
        &env.run(&[
            "doctor",
            "--repo",
            &repo,
            "--fix-stale-lock",
            "--external-writers-quiesced",
            "--json",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(foreign["findings"][0]["code"], "lock-unverified");
    assert!(lock_dir.exists());
    fs::remove_dir_all(&lock_dir).unwrap();

    fs::remove_dir_all(&path).unwrap();
    let missing: Value =
        serde_json::from_str(&env.ok(&["doctor", "--repo", &repo, "--json"])).unwrap();
    assert_eq!(missing["status"], "warnings");
    assert_eq!(missing["findings"][0]["code"], "worktree-missing");
    assert!(missing["findings"][0]["detail"]
        .as_str()
        .unwrap()
        .contains(&id));
}

#[test]
fn quarantine_lists_operations_and_prune_purges_them_by_age() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("prune-cache");
    env.exclude("/.ruff_cache/");
    fs::create_dir_all(path.join(".ruff_cache")).unwrap();
    fs::write(path.join(".ruff_cache/state"), b"cache state").unwrap();
    env.ok(&[
        "remove",
        "--repo",
        &repo,
        "--id",
        &id,
        "--preserve-generated-artifacts",
    ]);

    let listed: Value =
        serde_json::from_str(&env.ok(&["quarantine", "--repo", &repo, "--json"])).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["preserved_items"], 1);
    assert_eq!(listed[0]["age_days"], 0);
    let operation = listed[0]["operation_id"].as_str().unwrap().to_string();

    assert_eq!(
        env.run(&[
            "prune",
            "--repo",
            &repo,
            "--older-than-days",
            "1",
            "--quarantine"
        ])
        .status
        .code(),
        Some(4)
    );
    assert_eq!(
        env.run(&["prune", "--repo", &repo, "--older-than-days", "1"])
            .status
            .code(),
        Some(2)
    );
    let young = env.ok(&[
        "prune",
        "--repo",
        &repo,
        "--older-than-days",
        "1",
        "--quarantine",
        "--external-writers-quiesced",
    ]);
    assert!(young.contains("nothing older"));
    assert!(env.quarantine_root().join(&operation).exists());

    let dry = env.ok(&[
        "prune",
        "--repo",
        &repo,
        "--older-than-days",
        "0",
        "--quarantine",
        "--dry-run",
    ]);
    assert!(dry.contains("would-purge"), "{dry}");
    assert!(env.quarantine_root().join(&operation).exists());
    env.ok(&[
        "prune",
        "--repo",
        &repo,
        "--older-than-days",
        "0",
        "--quarantine",
        "--external-writers-quiesced",
    ]);
    assert!(!env.quarantine_root().join(&operation).exists());
}

#[test]
fn prune_deletes_kept_task_tips_only_when_asked_and_old_enough() {
    let env = Env::new();
    let repo = env.repo_arg();
    let (id, path) = env.create("prune-refs");
    let tip = commit_file(&path, "kept.txt", "kept\n");
    env.ok(&["remove", "--repo", &repo, "--id", &id, "--abandon"]);
    let ref_name = format!("refs/wtctl/abandoned/{id}");

    env.ok(&[
        "prune",
        "--repo",
        &repo,
        "--older-than-days",
        "0",
        "--quarantine",
        "--external-writers-quiesced",
    ]);
    assert_eq!(git(&env.repo, &["rev-parse", &ref_name]), tip);
    env.ok(&[
        "prune",
        "--repo",
        &repo,
        "--older-than-days",
        "1",
        "--refs",
        "--external-writers-quiesced",
    ]);
    assert_eq!(git(&env.repo, &["rev-parse", &ref_name]), tip);
    let dry = env.ok(&[
        "prune",
        "--repo",
        &repo,
        "--older-than-days",
        "0",
        "--refs",
        "--dry-run",
    ]);
    assert!(dry.contains("would-delete"));
    assert_eq!(git(&env.repo, &["rev-parse", &ref_name]), tip);
    env.ok(&[
        "prune",
        "--repo",
        &repo,
        "--older-than-days",
        "0",
        "--refs",
        "--external-writers-quiesced",
    ]);
    let gone = git_cmd(&env.repo, &["rev-parse", "--verify", "--quiet", &ref_name])
        .output()
        .unwrap();
    assert!(!gone.status.success());
}

#[test]
fn list_all_repos_groups_task_worktrees_from_every_repository() {
    let env = Env::new();
    let (first, _) = env.create("first-repo-task");
    let other = env.tmp.join("other");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    commit_file(&other, "o.txt", "other\n");
    let other = fs::canonicalize(&other).unwrap();
    let created: Value = serde_json::from_str(&env.ok(&[
        "create",
        "--repo",
        other.to_str().unwrap(),
        "--slug",
        "second-repo-task",
        "--json",
    ]))
    .unwrap();
    let second = created["task_id"].as_str().unwrap().to_string();

    let rows: Value = serde_json::from_str(&env.ok(&["list", "--all-repos", "--json"])).unwrap();
    let rows = rows.as_array().unwrap();
    let repo_of = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .map(|row| row["repo"].as_str().unwrap().to_string())
    };
    assert_eq!(repo_of(&first), Some(env.repo_arg()));
    assert_eq!(repo_of(&second), Some(other.to_string_lossy().into_owned()));
    let text = env.ok(&["list", "--all-repos"]);
    assert!(
        text.contains(&format!("{}\t{first}", env.repo_arg())),
        "{text}"
    );
    assert_eq!(
        env.run(&["list", "--all-repos", "--repo", &env.repo_arg()])
            .status
            .code(),
        Some(2)
    );
}
