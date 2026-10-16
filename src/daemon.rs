//! Running in the background (`-d`) and pid files.
//!
//! Daemonizing happens before the tokio runtime starts (forking a
//! multi-threaded process isn't safe), and after the config is checked and
//! sockets are bound, so startup errors still reach the terminal.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use nix::sys::signal::kill;
use nix::unistd::{ForkResult, Pid, chdir, dup2_stderr, dup2_stdin, dup2_stdout, fork, setsid};

/// Refuses to start if `path` names a process that's still running.
pub fn check_pid_file(path: &Path) -> Result<(), String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Ok(());
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return Ok(());
    };
    // Signal 0 only checks the process exists. EPERM means it exists but
    // belongs to someone else.
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) if pid != std::process::id() as i32 => Err(format!(
            "ferry is already running (pid {pid}, from {}); stop it or remove the pid file",
            path.display()
        )),
        _ => Ok(()),
    }
}

/// Writes this process's pid to `path`.
pub fn write_pid_file(path: &Path) -> Result<PidFile, String> {
    let err = |e: std::io::Error| format!("can't write pid file {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(err)?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)
        .map_err(err)?;
    writeln!(f, "{}", std::process::id()).map_err(err)?;
    Ok(PidFile(path.to_path_buf()))
}

/// Removes the pid file when dropped, if it still names this process.
pub struct PidFile(PathBuf);

impl Drop for PidFile {
    fn drop(&mut self) {
        let ours =
            fs::read_to_string(&self.0).is_ok_and(|s| s.trim() == std::process::id().to_string());
        if ours {
            let _ = fs::remove_file(&self.0);
        }
    }
}

/// Detaches from the terminal. Only the final background process returns;
/// the original exits once the background one reports it's ready, with its
/// exit status reflecting whether that went well.
///
/// `ready` runs in the background process (e.g. to write the pid file);
/// an error from it is printed by the original process.
pub fn daemonize<T>(ready: impl FnOnce() -> Result<T, String>) -> T {
    let (mut reader, mut writer) = std::io::pipe().expect("can create a pipe");

    // SAFETY: no other threads exist yet; the tokio runtime and file
    // watcher start after this.
    match unsafe { fork() }.expect("can fork") {
        ForkResult::Parent { .. } => {
            drop(writer);
            let mut report = String::new();
            let _ = reader.read_to_string(&mut report);
            match report.strip_prefix("ok") {
                Some(_) => std::process::exit(0),
                None if report.is_empty() => {
                    eprintln!("error: ferry exited while starting in the background");
                    std::process::exit(1);
                }
                None => {
                    eprintln!("error: {report}");
                    std::process::exit(1);
                }
            }
        }
        ForkResult::Child => {}
    }
    drop(reader);

    // A new session with no controlling terminal; then fork again so the
    // daemon isn't a session leader and can never acquire one.
    setsid().expect("can start a new session");
    // SAFETY: still single-threaded.
    if let ForkResult::Parent { .. } = unsafe { fork() }.expect("can fork") {
        std::process::exit(0);
    }

    let _ = chdir("/");
    if let Ok(null) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let _ = dup2_stdin(&null);
        let _ = dup2_stdout(&null);
        let _ = dup2_stderr(&null);
    }

    match ready() {
        Ok(value) => {
            let _ = writer.write_all(b"ok");
            value
        }
        Err(e) => {
            let _ = writer.write_all(e.as_bytes());
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_lifecycle() {
        let dir = std::env::temp_dir().join(format!("ferry-pid-{}", fastrand::u64(..)));
        let path = dir.join("run/ferry.pid");

        check_pid_file(&path).unwrap();
        let pid_file = write_pid_file(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        // Our own pid doesn't block us (e.g. a reload re-checking).
        check_pid_file(&path).unwrap();
        drop(pid_file);
        assert!(!path.exists());

        // A live process blocks; a dead one doesn't.
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        fs::write(&path, format!("{}\n", child.id())).unwrap();
        assert!(
            check_pid_file(&path)
                .unwrap_err()
                .contains("already running")
        );
        child.kill().unwrap();
        child.wait().unwrap();
        check_pid_file(&path).unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
