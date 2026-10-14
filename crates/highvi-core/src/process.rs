//! Running external tools with bounded output, timeouts and cancellation.
//!
//! Each child gets its own process group, and every exit path that does not
//! wait for a normal exit kills the whole group before reaping the leader, so
//! the group ID cannot be reused while it is being signalled.

use crate::{CappedOutput, read_capped};
use std::{ffi::OsStr, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{mpsc, watch},
    time::timeout,
};

const MAX_LINE_BYTES: usize = 16 * 1024;

/// Requests cancellation of the job that holds the matching [`CancelToken`].
#[derive(Debug)]
pub struct CancelHandle(watch::Sender<bool>);

#[derive(Clone, Debug)]
pub struct CancelToken(watch::Receiver<bool>);

pub fn cancel_pair() -> (CancelHandle, CancelToken) {
    let (sender, receiver) = watch::channel(false);
    (CancelHandle(sender), CancelToken(receiver))
}

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}

impl CancelToken {
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once cancellation is requested. Never resolves if the handle
    /// was dropped without cancelling.
    pub async fn cancelled(&mut self) {
        if self.0.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[derive(Debug)]
pub enum RunError {
    /// The executable could not be started (`NotFound` when it is missing).
    Spawn(std::io::Error),
    Io,
    TimedOut,
    Cancelled,
}

pub struct Captured {
    pub success: bool,
    pub stdout: CappedOutput,
    pub stderr: CappedOutput,
}

/// A command with no shell, no stdin, a cleared environment (plus `env`), and
/// its own process group.
pub fn tool_command<'a>(
    program: &Path,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    env: impl IntoIterator<Item = (&'a str, std::ffi::OsString)>,
) -> Command {
    let mut command = Command::new(program);
    command
        .kill_on_drop(true)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(env)
        .args(args);
    command
}

/// Runs to completion and keeps at most `stdout_limit`/`stderr_limit` bytes.
pub async fn run_captured(
    mut command: Command,
    stdout_limit: usize,
    stderr_limit: usize,
    limit: Duration,
    cancel: &mut CancelToken,
) -> Result<Captured, RunError> {
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().ok_or(RunError::Io)?;
    let stderr = child.stderr.take().ok_or(RunError::Io)?;

    let outcome = {
        let work = async {
            tokio::join!(
                child.wait(),
                read_capped(stdout, stdout_limit),
                read_capped(stderr, stderr_limit)
            )
        };
        tokio::select! {
            result = timeout(limit, work) => Some(result),
            () = cancel.cancelled() => None,
        }
    };
    match outcome {
        None => {
            kill_group(&mut child).await;
            Err(RunError::Cancelled)
        }
        Some(Err(_)) => {
            kill_group(&mut child).await;
            Err(RunError::TimedOut)
        }
        Some(Ok((status, stdout, stderr))) => {
            let status = status.map_err(|_| RunError::Io)?;
            Ok(Captured {
                success: status.success(),
                stdout: stdout.map_err(|_| RunError::Io)?,
                stderr: stderr.map_err(|_| RunError::Io)?,
            })
        }
    }
}

/// Runs to completion, passing each stdout/stderr line (lossily decoded,
/// oversized lines dropped) to `on_line`. Returns whether it exited with 0.
pub async fn run_streaming(
    mut command: Command,
    limit: Option<Duration>,
    cancel: &mut CancelToken,
    mut on_line: impl FnMut(&str),
) -> Result<bool, RunError> {
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().ok_or(RunError::Io)?;
    let stderr = child.stderr.take().ok_or(RunError::Io)?;

    let outcome = {
        let (sender, mut receiver) = mpsc::channel::<String>(256);
        let readers = async {
            tokio::join!(
                read_lines(stdout, sender.clone()),
                read_lines(stderr, sender)
            )
        };
        let consumer = async {
            while let Some(line) = receiver.recv().await {
                on_line(&line);
            }
        };
        let work = async {
            let (status, (), ()) = tokio::join!(
                child.wait(),
                async {
                    readers.await;
                },
                consumer
            );
            status
        };
        let work = async {
            match limit {
                Some(limit) => timeout(limit, work).await.ok(),
                None => Some(work.await),
            }
        };
        tokio::select! {
            result = work => Some(result),
            () = cancel.cancelled() => None,
        }
    };
    match outcome {
        None => {
            kill_group(&mut child).await;
            Err(RunError::Cancelled)
        }
        Some(None) => {
            kill_group(&mut child).await;
            Err(RunError::TimedOut)
        }
        Some(Some(status)) => Ok(status.map_err(|_| RunError::Io)?.success()),
    }
}

async fn read_lines<R: AsyncRead + Unpin>(mut reader: R, sender: mpsc::Sender<String>) {
    let mut chunk = vec![0_u8; 8 * 1024];
    let mut line = Vec::with_capacity(512);
    let mut oversized = false;
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if !oversized && sender.send(decode(&line)).await.is_err() {
                    return;
                }
                line.clear();
                oversized = false;
            } else if !oversized {
                if line.len() < MAX_LINE_BYTES {
                    line.push(*byte);
                } else {
                    line.clear();
                    oversized = true;
                }
            }
        }
    }
    if !line.is_empty() && !oversized {
        let _ = sender.send(decode(&line)).await;
    }
}

fn decode(line: &[u8]) -> String {
    String::from_utf8_lossy(line)
        .trim_end_matches('\r')
        .to_string()
}

/// SIGKILLs the child's process group, then reaps the leader.
pub async fn kill_group(child: &mut Child) {
    if let Some(pid) = child.id()
        && let Ok(pid) = libc::pid_t::try_from(pid)
    {
        // SAFETY: plain syscall; the leader has not been reaped yet, so
        // `pid` still names the group this child created.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Command {
        tool_command(
            Path::new("/bin/sh"),
            ["-c", script],
            [("PATH", std::ffi::OsString::from("/usr/bin:/bin"))],
        )
    }

    #[tokio::test]
    async fn streaming_delivers_lines_from_both_streams_and_drops_oversized_ones() {
        let (_handle, mut token) = cancel_pair();
        let mut lines = Vec::new();
        let ok = run_streaming(
            sh("echo one; echo two >&2; head -c 20000 /dev/zero | tr '\\0' x; echo; printf 'last\\r'"),
            Some(Duration::from_secs(10)),
            &mut token,
            |line| lines.push(line.to_string()),
        )
        .await
        .unwrap();
        assert!(ok);
        lines.sort();
        assert_eq!(lines, ["last", "one", "two"]);
    }

    #[tokio::test]
    async fn cancellation_kills_the_whole_process_group() {
        let (handle, mut token) = cancel_pair();
        let marker = std::env::temp_dir().join(format!("highvi-orphan-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "(sleep 2; touch '{}') & echo started; wait",
            marker.display()
        );
        let started = Instant::now();
        let run = run_streaming(sh(&script), None, &mut token, |line| {
            if line == "started" {
                handle.cancel();
            }
        });
        assert!(matches!(run.await, Err(RunError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "a grandchild survived cancellation");
    }

    #[tokio::test]
    async fn timeouts_and_missing_programs_are_reported() {
        let (_handle, mut token) = cancel_pair();
        let result = run_captured(
            sh("sleep 5"),
            10,
            10,
            Duration::from_millis(100),
            &mut token,
        )
        .await;
        assert!(matches!(result, Err(RunError::TimedOut)));

        let missing = tool_command(Path::new("/nonexistent/highvi-tool"), ["x"], []);
        let result = run_captured(missing, 10, 10, Duration::from_secs(1), &mut token).await;
        assert!(
            matches!(result, Err(RunError::Spawn(ref error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[tokio::test]
    async fn captured_output_is_bounded() {
        let (_handle, mut token) = cancel_pair();
        let captured = run_captured(
            sh("head -c 100000 /dev/zero; echo err >&2; exit 3"),
            1024,
            1024,
            Duration::from_secs(10),
            &mut token,
        )
        .await
        .unwrap();
        assert!(!captured.success);
        assert!(captured.stdout.overflowed);
        assert_eq!(captured.stdout.bytes.len(), 1024);
        assert_eq!(captured.stderr.bytes, b"err\n");
    }
}
