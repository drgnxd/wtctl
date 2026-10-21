//! Read-only task inspection (`show`) and marker metadata updates (`describe`).

use super::*;

const SHOWN_COMMITS: usize = 20;

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

pub fn session_arg(a: &Args) -> R<Option<String>> {
    let Some(id) = a.get("--session")? else {
        return Ok(None);
    };
    if !valid_session_id(id) {
        return Err(Fail::new(
            2,
            "--session must be 1-128 characters of letters, digits, '_', '-', '.', or ':'",
        ));
    }
    Ok(Some(id.to_string()))
}

/// The command that reopens the owning client's session, when it is known.
pub fn resume_command(marker: &Marker) -> Option<String> {
    let session = marker.session_hint().filter(|id| valid_session_id(id))?;
    match marker.owner.client.as_str() {
        "claude" => Some(format!("claude --resume {session}")),
        "opencode" => Some(format!("opencode --session {session}")),
        _ => None,
    }
}

fn log_lines(worktree: &Path, range: &str) -> Vec<String> {
    let limit = format!("-n{SHOWN_COMMITS}");
    git::run(
        worktree,
        ["log", "--format=%h %s", &limit, "--end-of-options", range],
    )
    .map(|text| text.lines().map(str::to_string).collect())
    .unwrap_or_default()
}

pub fn cmd_show(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let id = a.need("--id")?;
    let (entry, marker) = repo.find_task(id)?;
    let (_, main_ref) = base_ref(a, &repo.root)?;
    let facts = task::facts(&[&entry], &main_ref)?
        .into_iter()
        .next()
        .unwrap_or_default();
    let head = entry.current_oid();
    let exists = entry.path.is_dir();
    let (commits, shortstat) = match (&head, exists) {
        (Some(head), true) => (
            log_lines(&entry.path, &format!("{main_ref}..{head}")),
            git::run(
                &entry.path,
                [
                    "diff",
                    "--shortstat",
                    "--end-of-options",
                    &format!("{main_ref}...{head}"),
                ],
            )
            .unwrap_or_default(),
        ),
        _ => (Vec::new(), String::new()),
    };
    let state = procinfo::liveness(&marker.owner);
    let created = marker.raw.get("created_unix").and_then(Value::as_i64);
    let age_secs = created.map(|at| (now() - at).max(0));
    let removable = match facts.removable() {
        Ok(()) => Value::Bool(true),
        Err(why) => Value::String(why),
    };
    let task_text = marker.raw.get("task").and_then(Value::as_str);
    let parent = marker.raw.get("parent").and_then(Value::as_str);
    let history = marker
        .raw
        .get("history")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let report = json!({
        "task_id": marker.task_id,
        "task": task_text,
        "parent": parent,
        "path": entry.path,
        "exists": exists,
        "base": marker.base,
        "head": head,
        "created_unix": created,
        "age_secs": age_secs,
        "owner": owner_json(&marker.owner),
        "owner_state": state.as_str(),
        "session": marker.session_hint(),
        "resume_command": resume_command(&marker),
        "locked": entry.lock_reason.is_some(),
        "lock_reason": entry.lock_reason,
        "clean": facts.clean.map(|c| c.is_clean()),
        "changes": facts.clean.map(|c| json!({"tracked": c.tracked, "untracked": c.untracked, "ignored": c.ignored})),
        "in_progress": facts.in_progress,
        "blocker": facts.blocker,
        "unintegrated": facts.unintegrated,
        "commits": commits,
        "diffstat": shortstat.trim(),
        "removable": removable,
        "history": history,
    });
    if a.flag("--json") {
        out!("{report}");
        return Ok(());
    }
    out!("task_id={}", marker.task_id);
    if let Some(text) = task_text.filter(|text| !text.is_empty()) {
        out!("task={text}");
    }
    if let Some(parent) = parent {
        out!("parent={parent}");
    }
    out!(
        "path={}{}",
        entry.path.display(),
        if exists { "" } else { " (missing)" }
    );
    out!("base={}", marker.base);
    out!("head={}", head.as_deref().unwrap_or("?"));
    if let Some(age) = age_secs {
        out!("age={}h{:02}m", age / 3600, age % 3600 / 60);
    }
    out!(
        "owner={} pid={} state={}",
        marker.owner.client,
        marker.owner.pid,
        state.as_str()
    );
    if let Some(session) = marker.session_hint() {
        out!("session={session}");
    }
    if let Some(command) = resume_command(&marker) {
        out!("resume={command}");
    }
    out!("locked={}", entry.lock_reason.as_deref().unwrap_or("no"));
    if let Some(c) = facts.clean {
        out!(
            "changes=tracked:{} untracked:{} ignored:{}",
            c.tracked,
            c.untracked,
            c.ignored
        );
    }
    match facts.unintegrated {
        Some(n) => out!("unintegrated={n}"),
        None => out!("unintegrated=?"),
    }
    match &report["removable"] {
        Value::Bool(true) => out!("removable=yes"),
        Value::String(why) => out!("removable=no ({why})"),
        _ => {}
    }
    if !shortstat.trim().is_empty() {
        out!("diffstat={}", shortstat.trim());
    }
    for line in &commits {
        out!("commit {line}");
    }
    if commits.len() == SHOWN_COMMITS {
        out!("commit ... (showing the newest {SHOWN_COMMITS})");
    }
    Ok(())
}

pub fn cmd_describe(a: &Args) -> R<()> {
    let repo = a.repo(true)?;
    let id = a.need("--id")?;
    let task_text = a.get("--task")?;
    let session = session_arg(a)?;
    if task_text.is_none() && session.is_none() {
        return Err(Fail::new(2, "describe needs --task and/or --session"));
    }
    if task_text.is_some_and(|text| text.contains(['\n', '\r'])) {
        return Err(Fail::new(2, "--task must be a single line"));
    }
    let _lock = repo.lock()?;
    let (entry, marker) = repo.find_task(id)?;
    require_owner(&marker)?;
    let mut raw = marker.raw.clone();
    let event = json!({"event": "describe", "at": now()});
    match raw.get_mut("history").and_then(Value::as_array_mut) {
        Some(history) => history.push(event),
        None => raw["history"] = json!([event]),
    }
    if let Some(text) = task_text {
        raw["task"] = json!(text);
    }
    if let Some(session) = &session {
        raw["session_hint"] = json!(session);
    }
    task::write_marker(&entry.admin, &raw)?;
    repo.audit(json!({"at": now(), "event": "describe", "task_id": id}));
    if a.flag("--json") {
        out!("{raw}");
    } else {
        out!("described {id}");
    }
    Ok(())
}
