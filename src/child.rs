//! Running the app as a child process: `ferry 6005:6006 -- npm run dev`.
//!
//! The child shares ferry's terminal: it reads stdin and writes straight to
//! stdout and stderr. ferry passes termination signals on, and exits when
//! the child does, with its exit code.

use std::ffi::OsString;
use std::process::ExitStatus;
use std::sync::OnceLock;
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::net::{TcpStream, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::watch;

use crate::mapping::Upstream;

/// How long requests wait for the app to start, and how long the banner
/// waits before showing anyway.
pub const STARTUP_WAIT: Duration = Duration::from_secs(30);

/// `true` while the child is starting. Unset when there's no child.
static STARTING: OnceLock<watch::Sender<bool>> = OnceLock::new();

/// Starts `command`, with requests held until [`started`] is called.
pub fn spawn(command: &[OsString]) -> Result<Child, String> {
    let (program, args) = command.split_first().expect("a command has a program");
    let child = Command::new(program).args(args).spawn().map_err(|e| {
        format!(
            "can't run `{}`: {}",
            display(command),
            match e.kind() {
                std::io::ErrorKind::NotFound => "command not found".to_string(),
                _ => e.to_string(),
            }
        )
    })?;
    let _ = STARTING.set(watch::Sender::new(true));
    Ok(child)
}

/// Lets held requests through: the app is listening, it's been too long, or
/// it has exited.
pub fn started() {
    if let Some(starting) = STARTING.get() {
        starting.send_replace(false);
    }
}

/// Waits while the child is starting, so a request that arrives before the
/// app listens is answered by the app rather than with an error page.
pub async fn wait_until_started() {
    if let Some(starting) = STARTING.get() {
        let _ = starting.subscribe().wait_for(|starting| !starting).await;
    }
}

/// Waits until every upstream accepts connections, for up to `timeout`.
/// Returns those that still don't.
pub async fn wait_for_upstreams(upstreams: &[Upstream], timeout: Duration) -> Vec<Upstream> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut waiting: Vec<Upstream> = upstreams.to_vec();
    waiting.dedup();
    loop {
        let mut still = Vec::new();
        for upstream in waiting {
            if !accepts(&upstream).await {
                still.push(upstream);
            }
        }
        waiting = still;
        if waiting.is_empty() || tokio::time::Instant::now() >= deadline {
            return waiting;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn accepts(upstream: &Upstream) -> bool {
    match upstream {
        Upstream::Tcp { host, port } => TcpStream::connect((host.as_str(), *port)).await.is_ok(),
        Upstream::Unix(path) => UnixStream::connect(path).await.is_ok(),
    }
}

/// Passes a termination signal on to the child. Ctrl-C in the terminal
/// already reaches the child, which shares ferry's process group, so SIGINT
/// is only passed on when ferry isn't in the terminal's foreground (someone
/// ran `kill -INT`); otherwise the child would get it twice, which some dev
/// servers take as "force quit".
pub fn forward(child: &Child, sig: Signal) {
    let Some(pid) = child.id() else {
        return;
    };
    if sig == Signal::SIGINT && in_foreground() {
        return;
    }
    let _ = signal::kill(Pid::from_raw(pid as i32), sig);
}

fn in_foreground() -> bool {
    nix::unistd::tcgetpgrp(std::io::stdin()).is_ok_and(|fg| fg == nix::unistd::getpgrp())
}

/// Stops the child if it's still running: it was sent a signal; give it
/// `grace` to exit, then kill it.
pub async fn stop(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    if let Ok(Ok(status)) = tokio::time::timeout(grace, child.wait()).await {
        return Some(status);
    }
    tracing::warn!("the command didn't exit within {grace:?}; killing it");
    let _ = child.start_kill();
    child.wait().await.ok()
}

/// The exit code a shell would report for `status`: the child's own, or 128
/// plus the signal that killed it.
pub fn exit_code(status: ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code as u8,
        (None, Some(sig)) => 128u8.wrapping_add(sig as u8),
        (None, None) => 1,
    }
}

/// The command as people typed it.
pub fn display(command: &[OsString]) -> String {
    command
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn exit_codes_like_a_shell() {
        assert_eq!(exit_code(ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(ExitStatus::from_raw(15)), 143); // SIGTERM
        assert_eq!(exit_code(ExitStatus::from_raw(9)), 137); // SIGKILL
    }

    #[tokio::test]
    async fn waits_for_upstreams() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = Upstream::Tcp {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        };
        assert!(
            wait_for_upstreams(std::slice::from_ref(&up), Duration::ZERO)
                .await
                .is_empty()
        );
        drop(listener);
        let missing =
            wait_for_upstreams(std::slice::from_ref(&up), Duration::from_millis(200)).await;
        assert_eq!(missing, vec![up]);
    }
}
