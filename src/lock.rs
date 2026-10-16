//! Repository mutation lock: a kernel flock plus the legacy mkdir lock, so
//! wtctl and the legacy Python tools exclude each other in both directions.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::Fail;

pub const LEGACY_LOCK: &str = "main-integration.lock";
pub const FLOCK_PATH: &str = "agent-locks/integrate.lock";

pub struct Flock {
    _file: File,
}

pub fn flock(path: &Path) -> Result<Flock, Fail> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| Fail::new(1, format!("{}: {e}", dir.display())))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Fail::new(1, format!("{}: {e}", path.display())))?;
    // SAFETY: the descriptor is owned by `file` for the lifetime of the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        return Err(if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Fail::new(5, format!("{} is held by another process", path.display()))
        } else {
            Fail::new(1, format!("cannot lock {}: {err}", path.display()))
        });
    }
    Ok(Flock { _file: file })
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is writable for its full length.
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The legacy protocol: `mkdir` the lock, record `<token>\n<pid>\n<host>\n` in
/// `owner`, and release by renaming to `<name>.released.<token>` and deleting it.
pub struct LegacyLock {
    dir: PathBuf,
    token: String,
}

impl LegacyLock {
    pub fn acquire(common: &Path, token: String) -> Result<LegacyLock, Fail> {
        let dir = common.join(LEGACY_LOCK);
        if let Err(e) = fs::create_dir(&dir) {
            return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                Fail::new(5, format!("{} is held by another actor", dir.display()))
            } else {
                Fail::new(1, format!("cannot create {}: {e}", dir.display()))
            });
        }
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(dir.join("owner"))
            .and_then(|mut f| {
                f.write_all(
                    format!("{token}\n{}\n{}\n", std::process::id(), hostname()).as_bytes(),
                )?;
                f.sync_all()
            });
        if let Err(e) = written {
            let _ = fs::remove_file(dir.join("owner"));
            let _ = fs::remove_dir(&dir);
            return Err(Fail::new(1, format!("cannot record lock ownership: {e}")));
        }
        Ok(LegacyLock { dir, token })
    }
}

impl Drop for LegacyLock {
    fn drop(&mut self) {
        let owner = fs::read_to_string(self.dir.join("owner")).unwrap_or_default();
        if owner.lines().next() != Some(self.token.as_str()) {
            let _ = writeln!(
                std::io::stderr(),
                "warning: {} is no longer ours; leaving it in place",
                self.dir.display()
            );
            return;
        }
        let released = self
            .dir
            .with_file_name(format!("{LEGACY_LOCK}.released.{}", self.token));
        if let Err(e) = fs::rename(&self.dir, &released) {
            let _ = writeln!(
                std::io::stderr(),
                "warning: could not release {}: {e}",
                self.dir.display()
            );
            return;
        }
        let _ = fs::remove_file(released.join("owner"));
        if let Err(e) = fs::remove_dir(&released) {
            let _ = writeln!(
                std::io::stderr(),
                "warning: could not delete {}: {e}",
                released.display()
            );
        }
    }
}

/// Terminating signals stay blocked from taking the lock until the command has
/// written its result, so an interrupt never skips a lock release or an error.
pub static SIGNALS_BLOCKED: AtomicBool = AtomicBool::new(false);

fn terminating_signals() -> libc::sigset_t {
    // SAFETY: sigemptyset initializes the set before sigaddset reads it.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

/// Undoes the parent's signal block in a forked child before exec.
pub fn unblock_in_child() -> std::io::Result<()> {
    let set = terminating_signals();
    // SAFETY: sigprocmask is async-signal-safe and `set` is initialized.
    if unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// True when a blocked SIGINT, SIGTERM, or SIGHUP is waiting for delivery.
pub fn interrupt_pending() -> bool {
    // SAFETY: zeroed is a valid placeholder that sigpending overwrites.
    let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `pending` is valid for writing.
    if unsafe { libc::sigpending(&mut pending) } != 0 {
        return false;
    }
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
        .iter()
        // SAFETY: `pending` was initialized by sigpending.
        .any(|sig| unsafe { libc::sigismember(&pending, *sig) } == 1)
}

static PREVIOUS_MASK: Mutex<Option<libc::sigset_t>> = Mutex::new(None);

fn block_signals() {
    let Ok(mut previous) = PREVIOUS_MASK.lock() else {
        return;
    };
    if previous.is_some() {
        return;
    }
    let set = terminating_signals();
    // SAFETY: zeroed is a valid placeholder that pthread_sigmask overwrites.
    let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: both sets are valid for the call.
    if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old) } == 0 {
        *previous = Some(old);
        SIGNALS_BLOCKED.store(true, Ordering::SeqCst);
    }
}

/// Restores the mask saved when the first lock was taken; pending signals fire
/// here. Called once the command's output, including errors, is written.
pub fn restore_signals() {
    let Ok(mut previous) = PREVIOUS_MASK.lock() else {
        return;
    };
    if let Some(old) = previous.take() {
        SIGNALS_BLOCKED.store(false, Ordering::SeqCst);
        // SAFETY: `old` was filled by pthread_sigmask in block_signals.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
    }
}

pub struct RepoLock {
    legacy: Option<LegacyLock>,
    flock: Option<Flock>,
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        drop(self.legacy.take());
        drop(self.flock.take());
    }
}

pub fn acquire(common: &Path, token: String) -> Result<RepoLock, Fail> {
    let flock = flock(&common.join(FLOCK_PATH))?;
    block_signals();
    let legacy = LegacyLock::acquire(common, token)?;
    Ok(RepoLock {
        legacy: Some(legacy),
        flock: Some(flock),
    })
}
