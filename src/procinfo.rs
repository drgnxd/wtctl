//! Process identity without spawning `ps`: macOS libproc/sysctl, Linux /proc.

use std::path::Path;

const DEFAULT_AGENTS: &[&str] = &["claude", "opencode"];
const MAX_DEPTH: usize = 64;
// JavaScript launchers whose script argument identifies the agent.
const SCRIPT_HOSTS: &[&str] = &["node", "bun"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub pid: i32,
    pub start: String,
    pub client: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    Unknown,
}

impl Liveness {
    pub fn as_str(self) -> &'static str {
        match self {
            Liveness::Alive => "alive",
            Liveness::Dead => "dead",
            Liveness::Unknown => "unknown",
        }
    }
}

struct Info {
    ppid: i32,
    start: String,
    zombie: bool,
    names: Vec<String>,
}

fn basename(text: &str) -> &str {
    text.rsplit('/').next().unwrap_or(text)
}

#[cfg(target_os = "macos")]
fn exec_names(pid: i32) -> Vec<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: sysctl with a null buffer only reports the required size.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 8 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size];
    // SAFETY: buf holds `size` writable bytes.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    buf.truncate(size);
    let Some(rest) = buf.get(4..) else {
        return Vec::new();
    };
    // Layout: argc (int), exec path, NUL padding, argv[0], argv[1], ...
    rest.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .take(3)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(target_os = "macos")]
fn info(pid: i32) -> Option<Info> {
    let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a properly sized, writable proc_bsdinfo.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut bsd as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if read != size {
        return None;
    }
    let comm: Vec<u8> = bsd
        .pbi_comm
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    let mut names = exec_names(pid);
    names.push(String::from_utf8_lossy(&comm).into_owned());
    Some(Info {
        ppid: bsd.pbi_ppid as i32,
        start: format!("{}.{:06}", bsd.pbi_start_tvsec, bsd.pbi_start_tvusec),
        // SZOMB
        zombie: bsd.pbi_status == 5,
        names,
    })
}

#[cfg(target_os = "linux")]
fn info(pid: i32) -> Option<Info> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.get(stat.rfind(')')? + 1..)?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let start = format!("{}@{boot_id}", fields.get(19)?);
    let mut names = Vec::new();
    if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        names.extend(
            cmdline
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .take(2)
                .map(|s| String::from_utf8_lossy(s).into_owned()),
        );
    }
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        names.insert(0, exe.to_string_lossy().into_owned());
    }
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        names.push(comm.trim().to_string());
    }
    Some(Info {
        ppid,
        start,
        zombie: fields.first() == Some(&"Z"),
        names,
    })
}

/// Start time of a live (non-zombie) process, as recorded in task markers.
pub fn start_of(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    info(pid).filter(|i| !i.zombie).map(|i| i.start)
}

/// Process start in whole Unix seconds, used only for legacy `pid=<pid>@<epoch>` trailers.
pub fn start_epoch(pid: i32) -> Option<i64> {
    let start = start_of(pid)?;
    #[cfg(target_os = "linux")]
    {
        let ticks: i64 = start.split('@').next()?.parse().ok()?;
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let btime: i64 = stat
            .lines()
            .find_map(|l| l.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as i64;
        Some(btime + ticks / hz)
    }
    #[cfg(not(target_os = "linux"))]
    {
        start.split('.').next()?.parse().ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existence {
    Missing,
    Present,
    Forbidden,
}

pub fn is_zombie(pid: i32) -> bool {
    pid > 1 && info(pid).is_some_and(|i| i.zombie)
}

pub fn existence(pid: i32) -> Existence {
    // SAFETY: signal 0 only checks for existence and permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Existence::Present;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Existence::Missing,
        _ => Existence::Forbidden,
    }
}

fn comparable(start: &str) -> bool {
    !start.is_empty() && !start.ends_with('@')
}

/// Decides liveness conservatively: only a missing pid, a zombie, or a readable
/// but different start time proves the recorded owner is gone.
pub fn judge(exists: Existence, observed: Option<(&str, bool)>, recorded: &str) -> Liveness {
    match (exists, observed) {
        (Existence::Missing, _) => Liveness::Dead,
        (Existence::Forbidden, _) | (_, None) => Liveness::Unknown,
        (Existence::Present, Some((_, true))) => Liveness::Dead,
        (Existence::Present, Some((start, false))) => {
            if !comparable(start) || !comparable(recorded) {
                Liveness::Unknown
            } else if start == recorded {
                Liveness::Alive
            } else {
                Liveness::Dead
            }
        }
    }
}

pub fn liveness(owner: &Owner) -> Liveness {
    if owner.pid <= 1 {
        return Liveness::Unknown;
    }
    let exists = existence(owner.pid);
    let observed = info(owner.pid);
    judge(
        exists,
        observed.as_ref().map(|i| (i.start.as_str(), i.zombie)),
        &owner.start,
    )
}

pub struct Cwd {
    pub pid: String,
    pub path: Vec<u8>,
    /// False when the reporter escaped bytes, so `path` may not be the real path.
    pub exact: bool,
}

pub enum CwdScan {
    Paths(Vec<Cwd>),
    Unavailable(String),
}

fn find_lsof() -> Option<std::path::PathBuf> {
    let from_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("lsof"));
    from_path
        .chain(["/usr/sbin/lsof", "/usr/bin/lsof"].map(std::path::PathBuf::from))
        .find(|p| p.is_file())
}

// lsof escapes unprintable and non-ASCII bytes (`\xNN`, `\n`, ...); only
// printable ASCII without a backslash is guaranteed to be the literal path.
fn literal_len(path: &[u8]) -> usize {
    path.iter()
        .position(|b| !(0x20..=0x7e).contains(b) || *b == b'\\')
        .unwrap_or(path.len())
}

/// Parses `lsof -Fpn` output strictly. The caller's own record is a sentinel:
/// lsof always sees wtctl itself, so its absence proves the listing incomplete.
/// Its presence does not prove completeness; processes lsof may not inspect are
/// silently missing.
pub fn interpret_lsof(stdout: &[u8], signaled: bool, self_pid: u32) -> CwdScan {
    if signaled {
        return CwdScan::Unavailable("lsof was killed by a signal".into());
    }
    let mut pid = None;
    let mut hits = Vec::new();
    for line in stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        match line.split_first() {
            Some((b'p', rest)) => pid = Some(String::from_utf8_lossy(rest).into_owned()),
            Some((b'f', _)) => {}
            Some((b'n', rest)) => {
                let Some(pid) = pid.clone() else {
                    return CwdScan::Unavailable("lsof printed a name before a pid".into());
                };
                hits.push(Cwd {
                    pid,
                    path: rest.to_vec(),
                    exact: literal_len(rest) == rest.len(),
                });
            }
            _ => return CwdScan::Unavailable("unexpected lsof output format".into()),
        }
    }
    with_sentinel(hits, self_pid, "lsof")
}

fn with_sentinel(hits: Vec<Cwd>, self_pid: u32, source: &str) -> CwdScan {
    let me = self_pid.to_string();
    if !hits.iter().any(|c| c.pid == me) {
        return CwdScan::Unavailable(format!("{source} did not report wtctl's own cwd"));
    }
    CwdScan::Paths(hits)
}

const LSOF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn lsof_cwds() -> CwdScan {
    let Some(lsof) = find_lsof() else {
        return CwdScan::Unavailable("lsof not found".into());
    };
    lsof_cwds_at(&lsof, LSOF_TIMEOUT)
}

/// Runs `lsof` with a deadline; a hung or interrupted scan counts as unavailable.
pub fn lsof_cwds_at(lsof: &Path, timeout: std::time::Duration) -> CwdScan {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    let mut child = match std::process::Command::new(lsof)
        .args(["-w", "-d", "cwd", "-Fpn"])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return CwdScan::Unavailable(format!("cannot run lsof: {e}")),
    };
    let Some(mut stdout) = child.stdout.take() else {
        return CwdScan::Unavailable("lsof stdout unavailable".into());
    };
    // Drain stdout concurrently so a large listing cannot block lsof on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return CwdScan::Unavailable(format!("cannot wait for lsof: {e}")),
        }
        let why = if std::time::Instant::now() >= deadline {
            Some("lsof timed out")
        } else if crate::lock::interrupt_pending() {
            Some("interrupted while waiting for lsof")
        } else {
            None
        };
        if let Some(why) = why {
            let _ = child.kill();
            let _ = child.wait();
            return CwdScan::Unavailable(why.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let Ok(Ok(stdout)) = reader.join() else {
        return CwdScan::Unavailable("cannot read lsof output".into());
    };
    // lsof exits 1 when some processes are unreadable, so the exit code alone
    // proves nothing; the sentinel and format checks decide.
    interpret_lsof(&stdout, status.signal().is_some(), std::process::id())
}

/// The cwd of `pid` as raw bytes from the kernel, without lsof's escaping.
pub fn cwd_of(pid: &str) -> Option<Vec<u8>> {
    let pid: i32 = pid.parse().ok().filter(|p| *p > 0)?;
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: the buffer is a properly sized, writable proc_vnodepathinfo.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if read != size {
            return None;
        }
        // SAFETY: vip_path is a contiguous, NUL-terminated MAXPATHLEN buffer.
        let raw: &[u8] = unsafe {
            std::slice::from_raw_parts(
                info.pvi_cdir.vip_path.as_ptr() as *const u8,
                std::mem::size_of_val(&info.pvi_cdir.vip_path),
            )
        };
        let end = raw.iter().position(|b| *b == 0)?;
        (end > 0).then(|| raw[..end].to_vec())
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let link = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        Some(link.as_os_str().as_bytes().to_vec())
    }
}

/// Reads `<root>/<pid>/cwd` links, as Linux exposes them under `/proc`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn proc_cwds(root: &Path, self_pid: u32) -> CwdScan {
    use std::os::unix::ffi::OsStrExt;
    let Ok(dir) = std::fs::read_dir(root) else {
        return CwdScan::Unavailable(format!("cannot read {}", root.display()));
    };
    let hits: Vec<Cwd> = dir
        .flatten()
        .filter(|e| e.file_name().as_bytes().iter().all(u8::is_ascii_digit))
        .filter_map(|e| {
            let target = std::fs::read_link(e.path().join("cwd")).ok()?;
            Some(Cwd {
                pid: e.file_name().to_string_lossy().into_owned(),
                path: target.as_os_str().as_bytes().to_vec(),
                exact: true,
            })
        })
        .collect();
    with_sentinel(hits, self_pid, &format!("{}/*/cwd", root.display()))
}

/// Current working directories of all visible processes.
pub fn scan_cwds() -> CwdScan {
    let scan = lsof_cwds();
    #[cfg(target_os = "linux")]
    if let CwdScan::Unavailable(why) = &scan {
        return match proc_cwds(Path::new("/proc"), std::process::id()) {
            CwdScan::Unavailable(also) => CwdScan::Unavailable(format!("{why}; {also}")),
            found => found,
        };
    }
    scan
}

fn fold(bytes: &[u8], case_insensitive: bool) -> Vec<u8> {
    if case_insensitive {
        bytes.to_ascii_lowercase()
    } else {
        bytes.to_vec()
    }
}

fn within(path: &[u8], dir: &[u8]) -> bool {
    path.starts_with(dir) && (path.len() == dir.len() || path.get(dir.len()) == Some(&b'/'))
}

/// Pids whose cwd is `dir` or below it; `dir` must be canonical. Escaped lsof
/// entries are re-read from the kernel; one that cannot be re-read and could
/// name `dir` makes the answer unknowable, so it fails closed.
pub fn users_inside(scan: &CwdScan, dir: &Path) -> Result<Vec<String>, String> {
    // macOS volumes usually ignore case and Unicode normalization; both
    // allowances only over-match.
    let macos = cfg!(target_os = "macos");
    users_inside_with(scan, dir, macos, macos, &cwd_of)
}

pub fn users_inside_with(
    scan: &CwdScan,
    dir: &Path,
    case_insensitive: bool,
    unicode_insensitive: bool,
    resolve: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Result<Vec<String>, String> {
    use std::os::unix::ffi::OsStrExt;
    let cwds = match scan {
        CwdScan::Paths(cwds) => cwds,
        CwdScan::Unavailable(why) => return Err(why.clone()),
    };
    let raw_dir = dir.as_os_str().as_bytes();
    let trimmed = raw_dir.strip_suffix(b"/").unwrap_or(raw_dir);
    let dir_bytes = fold(trimmed, case_insensitive);
    // Under normalization-insensitive lookup the bytes after the first
    // non-ASCII byte of either path may differ, and so may the ASCII base
    // character a combining mark attaches to.
    let dir_ascii = &dir_bytes[..dir_bytes
        .iter()
        .position(|b| !b.is_ascii())
        .unwrap_or(dir_bytes.len())];
    let mut pids = Vec::new();
    for cwd in cwds {
        let raw = if cwd.exact {
            Some(cwd.path.clone())
        } else {
            resolve(&cwd.pid)
        };
        if let Some(raw) = raw {
            if within(&fold(&raw, case_insensitive), &dir_bytes) {
                pids.push(cwd.pid.clone());
            }
            continue;
        }
        let path = fold(&cwd.path, case_insensitive);
        let mut literal = &path[..literal_len(&cwd.path)];
        let related = if unicode_insensitive {
            literal = &literal[..literal.len().saturating_sub(1)];
            dir_ascii.starts_with(literal) || literal.starts_with(dir_ascii)
        } else {
            dir_bytes.starts_with(literal) || literal.starts_with(&dir_bytes)
        };
        if related {
            return Err(format!(
                "pid {} has an escaped cwd that may lie inside {}",
                cwd.pid,
                dir.display()
            ));
        }
    }
    Ok(pids)
}

fn agent_names() -> Vec<String> {
    #[cfg(feature = "test-agent-names")]
    if let Ok(v) = std::env::var("WTCTL_AGENT_NAMES") {
        return v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    DEFAULT_AGENTS.iter().map(|s| s.to_string()).collect()
}

fn classify(names: &[String], agents: &[String]) -> Option<String> {
    let host = names.iter().any(|n| SCRIPT_HOSTS.contains(&basename(n)));
    for raw in names {
        let path = Path::new(raw);
        let base = basename(raw);
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        let grandparent = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        for agent in agents {
            // Nix wraps binaries as `.<name>-wrapped`, and the kernel truncates comm.
            let wrapped = format!(".{agent}-wrap");
            if base == agent || base.starts_with(&wrapped) {
                return Some(agent.clone());
            }
            // Self-updating installs exec a versioned file such as `<agent>/versions/<x.y.z>`.
            if parent == Some("versions") && grandparent == Some(agent.as_str()) {
                return Some(agent.clone());
            }
            // npm installs run `node <prefix>/<agent>-code/cli.js` or `node .../<agent>/...`.
            if host
                && (parent == Some(agent.as_str())
                    || parent.is_some_and(|p| p.strip_suffix("-code") == Some(agent)))
            {
                return Some(agent.clone());
            }
        }
    }
    None
}

/// Nearest ancestor process that is a known coding-agent client.
pub fn find_agent() -> Option<Owner> {
    let agents = agent_names();
    // SAFETY: getppid has no preconditions.
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..MAX_DEPTH {
        if pid <= 1 {
            return None;
        }
        let i = info(pid)?;
        if let Some(client) = classify(&i.names, &agents) {
            return (!i.zombie).then_some(Owner {
                pid,
                start: i.start,
                client,
            });
        }
        pid = i.ppid;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn classifies_known_launch_shapes() {
        let agents = s(DEFAULT_AGENTS);
        let cases: &[(&[&str], Option<&str>)] = &[
            (&["/opt/agent/bin/claude"], Some("claude")),
            (&["/x/share/claude/versions/2.1.0", "2.1.0"], Some("claude")),
            (&[".opencode-wrappe"], Some("opencode")),
            (
                &[
                    "node",
                    "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                ],
                Some("claude"),
            ),
            (&["/bin/bash", "bash"], None),
            (&["/usr/bin/python3", "/srv/claude-code/cli.py"], None),
        ];
        for (names, want) in cases {
            assert_eq!(classify(&s(names), &agents).as_deref(), *want, "{names:?}");
        }
    }

    #[test]
    fn liveness_is_dead_only_on_positive_evidence() {
        use Existence::*;
        assert_eq!(judge(Missing, None, "1.0"), Liveness::Dead);
        assert_eq!(judge(Present, Some(("1.0", false)), "1.0"), Liveness::Alive);
        assert_eq!(judge(Present, Some(("2.0", false)), "1.0"), Liveness::Dead);
        assert_eq!(judge(Present, Some(("1.0", true)), "1.0"), Liveness::Dead);
        assert_eq!(
            judge(Forbidden, Some(("2.0", false)), "1.0"),
            Liveness::Unknown
        );
        assert_eq!(judge(Present, None, "1.0"), Liveness::Unknown);
        assert_eq!(judge(Present, Some(("5@", false)), "4@"), Liveness::Unknown);
        assert_eq!(
            judge(Present, Some(("5@b", false)), "4@"),
            Liveness::Unknown
        );
        assert_eq!(judge(Present, Some(("5@b", false)), "4@b"), Liveness::Dead);
    }

    fn paths(scan: &CwdScan, dir: &str, fold: bool) -> Result<Vec<String>, String> {
        users_inside_with(scan, Path::new(dir), fold, false, &|_| None)
    }

    #[test]
    fn lsof_output_is_trusted_only_when_well_formed_and_complete() {
        let me = b"p7\nfcwd\nn/var/empty\n";
        let near = [
            &me[..],
            b"p10\nfcwd\nn/work/tree/sub\np12\nn/work/tr\\x65e\n",
        ]
        .concat();
        assert!(paths(&interpret_lsof(&near, false, 7), "/work/tree", false).is_err());
        let far = [
            &me[..],
            b"p10\nfcwd\nn/work/tree/sub\np11\nn/tmp/caf\\xc3\\xa9\n",
        ]
        .concat();
        let scan = interpret_lsof(&far, false, 7);
        assert_eq!(
            paths(&scan, "/work/tree", false).unwrap(),
            vec!["10".to_string()]
        );
        assert!(paths(&scan, "/work/tre", false).unwrap().is_empty());
        assert!(paths(&scan, "/elsewhere", false).unwrap().is_empty());
        // Missing sentinel, a signal-killed lsof, and foreign formats fail closed.
        assert!(paths(&interpret_lsof(&far, false, 8), "/w", false).is_err());
        assert!(paths(&interpret_lsof(&far, true, 7), "/w", false).is_err());
        let busybox = b"1234\t/bin/sh\t/dev/pts/0\n7\t/bin/wtctl\t/var/empty\n";
        assert!(paths(&interpret_lsof(busybox, false, 7), "/w", false).is_err());
        assert!(paths(&interpret_lsof(b"", false, 7), "/w", false).is_err());
        assert!(paths(&interpret_lsof(b"n/x\np7\nn/y\n", false, 7), "/w", false).is_err());
        assert!(paths(&CwdScan::Unavailable("x".into()), "/w", false).is_err());
    }

    #[test]
    fn escaped_entries_are_re_read_from_the_kernel() {
        let raw =
            b"p7\nn/var/empty\np20\nn/srv/\\xe6\\x97\\xa5\np21\nn/work/tree/\\xe6\\x97\\xa5\n";
        let scan = interpret_lsof(raw, false, 7);
        let resolve = |pid: &str| match pid {
            "20" => Some("/srv/日".as_bytes().to_vec()),
            "21" => Some("/work/tree/日".as_bytes().to_vec()),
            _ => None,
        };
        let dir = Path::new("/work/tree");
        assert_eq!(
            users_inside_with(&scan, dir, false, false, &resolve).unwrap(),
            vec!["21".to_string()]
        );
        let elsewhere = Path::new("/srv/data");
        assert!(users_inside_with(&scan, elsewhere, false, false, &resolve)
            .unwrap()
            .is_empty());
        // Unresolvable and possibly related: fail closed.
        assert!(users_inside_with(&scan, dir, false, false, &|_| None).is_err());
        // A worktree with a non-ASCII name only matches through the kernel path.
        let jp = Path::new("/srv/日");
        assert_eq!(
            users_inside_with(&scan, jp, true, true, &resolve).unwrap(),
            vec!["20".to_string()]
        );
        assert!(users_inside_with(&scan, jp, true, true, &|_| None).is_err());
        // Decomposed `e` + U+0301 versus a precomposed name: the base letter is
        // dropped before comparing, so the entry is treated as related.
        let decomposed = interpret_lsof(b"p7\nn/var/empty\np30\nn/w/e\\xcc\\x81\n", false, 7);
        assert!(users_inside_with(&decomposed, Path::new("/w/é"), false, true, &|_| None).is_err());
    }

    #[test]
    fn own_cwd_is_readable_from_the_kernel() {
        use std::os::unix::ffi::OsStrExt;
        let me = std::process::id().to_string();
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(cwd_of(&me).unwrap(), cwd.as_os_str().as_bytes());
        assert_eq!(cwd_of("0"), None);
    }

    #[test]
    fn hung_lsof_times_out() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("wtctl-lsof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("lsof");
        std::fs::write(&shim, "#!/bin/sh\nexec sleep 5\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let scan = lsof_cwds_at(&shim, std::time::Duration::from_millis(200));
        assert!(matches!(scan, CwdScan::Unavailable(ref why) if why == "lsof timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn case_folding_over_matches_on_case_insensitive_volumes() {
        let scan = interpret_lsof(b"p7\nn/Srv/Data/WT/sub\n", false, 7);
        assert_eq!(
            paths(&scan, "/srv/data/wt", true).unwrap(),
            vec!["7".to_string()]
        );
        assert!(paths(&scan, "/srv/data/wt", false).unwrap().is_empty());
        let escaped = interpret_lsof(b"p7\nn/var/empty\np8\nn/SRV/data/w\\x74\n", false, 7);
        assert!(paths(&escaped, "/srv/data/wt", true).is_err());
    }

    #[test]
    fn proc_style_cwd_links_need_the_own_entry() {
        let root = std::env::temp_dir().join(format!("wtctl-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (pid, target) in [("41", "/work/tree/a"), ("42", "/other")] {
            std::fs::create_dir_all(root.join(pid)).unwrap();
            std::os::unix::fs::symlink(target, root.join(pid).join("cwd")).unwrap();
        }
        std::fs::create_dir_all(root.join("self-not-a-pid")).unwrap();
        assert_eq!(
            paths(&proc_cwds(&root, 42), "/work/tree", false).unwrap(),
            vec!["41".to_string()]
        );
        assert!(paths(&proc_cwds(&root, 43), "/work/tree", false).is_err());
        std::fs::remove_dir_all(&root).unwrap();
        assert!(matches!(proc_cwds(&root, 42), CwdScan::Unavailable(_)));
    }

    #[test]
    fn own_process_is_alive_and_reaped_pid_is_dead() {
        let me = std::process::id() as i32;
        let start = start_of(me).expect("own start");
        let owner = |pid, start: &str| Owner {
            pid,
            start: start.to_string(),
            client: "x".into(),
        };
        assert_eq!(liveness(&owner(me, &start)), Liveness::Alive);
        assert_eq!(liveness(&owner(me, &format!("{start}0"))), Liveness::Dead);
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(liveness(&owner(pid, &start)), Liveness::Dead);
    }
}
