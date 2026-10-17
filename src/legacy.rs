use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};

const OPENCODE_SESSION_ACTIVITY: &str = r#"
WITH RECURSIVE up(id, parent_id, t) AS (
    SELECT id, parent_id, time_updated FROM session WHERE id = ?1
    UNION
    SELECT s.id, s.parent_id, s.time_updated
    FROM session s JOIN up ON s.id = up.parent_id
), down(id, t) AS (
    SELECT id, time_updated FROM session WHERE id = ?1
    UNION
    SELECT s.id, s.time_updated
    FROM session s JOIN down ON s.parent_id = down.id
)
SELECT max(t) FROM (SELECT t FROM up UNION ALL SELECT t FROM down)
"#;

fn valid_session(client: &str, session: &str) -> bool {
    match client {
        "claude" => {
            session.len() == 36
                && session.bytes().enumerate().all(|(i, b)| match i {
                    8 | 13 | 18 | 23 => b == b'-',
                    _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
                })
        }
        "opencode" => session
            .strip_prefix("ses_")
            .is_some_and(|v| v.len() == 26 && v.bytes().all(|b| b.is_ascii_alphanumeric())),
        _ => false,
    }
}

fn modified_seconds(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

pub fn claude_last_activity(
    session: &str,
    home: &Path,
    xdg_data_home: &Path,
    config_dir: Option<&Path>,
) -> Option<f64> {
    if !valid_session("claude", session) {
        return None;
    }
    let mut roots = Vec::new();
    if let Some(root) = config_dir {
        roots.push(root.to_path_buf());
    }
    roots.push(home.join(".claude"));
    roots.push(xdg_data_home.join("claude"));
    roots
        .into_iter()
        .filter_map(|root| fs::read_dir(root.join("projects")).ok())
        .flat_map(|entries| entries.flatten().map(|e| e.path()))
        .filter_map(|project| modified_seconds(&project.join(format!("{session}.jsonl"))))
        .max_by(f64::total_cmp)
}

fn opencode_db_activity(db: &Path, session: &str) -> Option<f64> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.busy_timeout(Duration::from_secs(2)).ok()?;
    let updated: Option<f64> = conn
        .query_row(OPENCODE_SESSION_ACTIVITY, params![session], |row| {
            row.get(0)
        })
        .ok()?;
    updated.map(|milliseconds| milliseconds / 1000.0)
}

pub fn opencode_last_activity(session: &str, xdg_data_home: &Path) -> Option<f64> {
    if !valid_session("opencode", session) {
        return None;
    }
    ["opencode.db", "opencode-stable.db"]
        .into_iter()
        .filter_map(|name| {
            opencode_db_activity(&xdg_data_home.join("opencode").join(name), session)
        })
        .max_by(f64::total_cmp)
}

pub fn session_idle_hours(
    client: &str,
    session: &str,
    home: &Path,
    xdg_data_home: &Path,
    claude_config_dir: Option<&Path>,
    now_seconds: f64,
) -> Option<f64> {
    let last = match client {
        "claude" => claude_last_activity(session, home, xdg_data_home, claude_config_dir),
        "opencode" => opencode_last_activity(session, xdg_data_home),
        _ => None,
    }?;
    Some(((now_seconds - last).max(0.0)) / 3600.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("wtctl-legacy-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn claude_activity_uses_only_the_matching_session_file() {
        let root = temp("claude");
        let home = root.join("home");
        let xdg = root.join("xdg");
        let project = home.join(".claude/projects/project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("0e7f96a6-1234-4abc-8def-0123456789ab.jsonl"),
            "session",
        )
        .unwrap();
        fs::write(
            project.join("9e7f96a6-1234-4abc-8def-0123456789ab.jsonl"),
            "other",
        )
        .unwrap();
        let last = claude_last_activity("0e7f96a6-1234-4abc-8def-0123456789ab", &home, &xdg, None)
            .unwrap();
        let now = UNIX_EPOCH.elapsed().unwrap().as_secs_f64();
        assert!(
            session_idle_hours(
                "claude",
                "0e7f96a6-1234-4abc-8def-0123456789ab",
                &home,
                &xdg,
                None,
                now
            )
            .unwrap()
                < 1.0
        );
        assert!(last <= now);
        assert!(claude_last_activity("invalid", &home, &xdg, None).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opencode_activity_reads_parent_and_child_sessions_read_only() {
        let root = temp("opencode");
        let xdg = root.join("xdg");
        let app = xdg.join("opencode");
        fs::create_dir_all(&app).unwrap();
        let db = app.join("opencode.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE session(id TEXT PRIMARY KEY, parent_id TEXT, time_updated INTEGER)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES (?1, NULL, ?2)",
            params!["ses_aaaaaaaaaaaaaaaaaaaaaaaaaa", 1_700_000_000_000i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES (?1, ?2, ?3)",
            params![
                "ses_bbbbbbbbbbbbbbbbbbbbbbbbbb",
                "ses_aaaaaaaaaaaaaaaaaaaaaaaaaa",
                1_700_000_001_000i64
            ],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            opencode_last_activity("ses_aaaaaaaaaaaaaaaaaaaaaaaaaa", &xdg),
            Some(1_700_000_001.0)
        );
        assert_eq!(
            opencode_last_activity("ses_bbbbbbbbbbbbbbbbbbbbbbbbbb", &xdg),
            Some(1_700_000_001.0)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
