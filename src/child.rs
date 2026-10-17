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

/// Which upstreams have started listening, while the command is starting.
#[derive(Default)]
struct Startup {
    up: Vec<Upstream>,
    /// The command has exited, or it's been [`STARTUP_WAIT`]: stop holding
    /// requests.
    over: bool,
}

/// Set when ferry runs a command.
static STARTUP: OnceLock<watch::Sender<Startup>> = OnceLock::new();

/// Starts `command`. Requests are held until their upstream listens; see
/// [`wait_for`] and [`watch_startup`].
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
    let _ = STARTUP.set(watch::Sender::new(Startup::default()));
    Ok(child)
}

/// Stops holding requests: the command has exited, or it's been too long.
pub fn started() {
    if let Some(startup) = STARTUP.get() {
        startup.send_modify(|s| s.over = true);
    }
}

/// Waits while the command is starting and `upstream` isn't listening yet,
/// so a request that arrives early is answered by the app rather than with
/// an error page. Each request waits only for its own upstream.
pub async fn wait_for(upstream: &Upstream) {
    if let Some(startup) = STARTUP.get() {
        let _ = startup
            .subscribe()
            .wait_for(|s| s.over || s.up.contains(upstream))
            .await;
    }
}

/// Watches `upstreams` while the command starts: prints `banner` once the
/// first one is listening (or after [`STARTUP_WAIT`] if none is), lets
/// requests for each through as it starts listening, and after
/// [`STARTUP_WAIT`] stops holding requests and warns about any still silent.
pub async fn watch_startup(upstreams: Vec<Upstream>, banner: Option<String>) {
    let Some(startup) = STARTUP.get() else {
        return;
    };
    let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
    let mut waiting = upstreams;
    waiting.dedup();
    let mut banner = banner;
    let mut any_up = false;
    loop {
        let mut still = Vec::new();
        for upstream in waiting {
            if accepts(&upstream).await {
                any_up = true;
                startup.send_modify(|s| s.up.push(upstream));
            } else {
                still.push(upstream);
            }
        }
        waiting = still;
        let timed_out = tokio::time::Instant::now() >= deadline;
        if (any_up || timed_out)
            && let Some(banner) = banner.take()
        {
            eprint!("{banner}");
        }
        if waiting.is_empty() || timed_out || startup.borrow().over {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    started();
    for upstream in waiting {
        tracing::warn!(
            "nothing is listening on {upstream} after {STARTUP_WAIT:?}; is that where your \
             app listens?"
        );
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
    async fn accepts_only_listening_upstreams() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = Upstream::Tcp {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        };
        assert!(accepts(&up).await);
        drop(listener);
        assert!(!accepts(&up).await);
    }
}
