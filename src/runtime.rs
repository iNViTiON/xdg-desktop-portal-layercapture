//! Process identity: the `layercapture` process name, the PID file and the `release` command.
//!
//! The process name makes the emergency hatch `pkill -KILL -x layercapture` unambiguous (the
//! default name would be truncated to `xdg-desktop-por`, the same as xdg-desktop-portal's own).
//! The PID file lets `xdg-desktop-portal-layercapture release` (e.g. from a niri bind with
//! `allow-inhibiting=false`) ask a running instance to drop any capture via SIGUSR1.

use std::ffi::CStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, getpid, kill_process};

pub const PROCESS_NAME: &CStr = c"layercapture";

/// Sets the name of the calling thread. Called on the main thread before any other thread is
/// spawned, so it becomes the process name that `pkill -x` matches.
pub fn set_process_name() {
    if let Err(e) = rustix::thread::set_name(PROCESS_NAME) {
        tracing::warn!("could not set process name: {e}");
    }
}

fn runtime_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(base).join("layercapture"))
}

fn pid_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("pid"))
}

/// True if `pid` is a live process whose name is ours.
fn is_layercapture(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim_end() == PROCESS_NAME.to_str().unwrap_or_default())
        .unwrap_or(false)
}

fn read_pid() -> Result<Option<i32>> {
    let path = pid_path()?;
    match fs::read_to_string(&path) {
        Ok(s) => Ok(s.trim().parse().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Owns the single-instance lock and the PID file; removes the PID file on drop if it still
/// holds our PID.
pub struct PidFile {
    path: PathBuf,
    pid: i32,
    /// flock()ed for the whole process lifetime; the kernel drops it on any exit, even SIGKILL.
    _lock: fs::File,
}

impl PidFile {
    /// Takes the single-instance lock and writes our PID. Two instances would fight over the
    /// same barrier and grab, and `release` would only reach one of them.
    pub fn create() -> Result<Self> {
        let dir = runtime_dir()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let lock_path = dir.join("lock");
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        if let Err(e) = rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            if e == rustix::io::Errno::WOULDBLOCK {
                bail!("another layercapture instance is running (pid {:?})", read_pid().ok().flatten());
            }
            return Err(e).context("locking the instance lock");
        }
        let pid = getpid().as_raw_nonzero().get();
        let path = pid_path()?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        writeln!(f, "{pid}")?;
        Ok(Self { path, pid, _lock: lock })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if let Ok(s) = fs::read_to_string(&self.path)
            && s.trim().parse::<i32>().ok() == Some(self.pid)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// `release` subcommand: SIGUSR1 to the running instance, after checking the PID really is
/// ours (a stale PID file after a crash must never signal an unrelated process).
pub fn send_release() -> ExitCode {
    let pid = match read_pid() {
        Ok(Some(pid)) => pid,
        Ok(None) => {
            eprintln!("layercapture: no running instance (no PID file)");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("layercapture: {e:#}");
            return ExitCode::from(1);
        }
    };
    if !is_layercapture(pid) {
        eprintln!("layercapture: PID {pid} is not a running layercapture instance (stale PID file)");
        return ExitCode::from(1);
    }
    let Some(pid) = Pid::from_raw(pid) else {
        eprintln!("layercapture: invalid PID {pid}");
        return ExitCode::from(1);
    };
    match kill_process(pid, Signal::USR1) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("layercapture: signalling PID {}: {e}", pid.as_raw_nonzero());
            ExitCode::from(1)
        }
    }
}
