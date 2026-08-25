//! Starting one child, bounding what it produces, and making sure it is gone afterwards.
//!
//! Three things go wrong when a process is run naively from a supervisor, and each is handled
//! here rather than left to the caller:
//!
//! 1. **Unbounded output.** `Command::output` reads until EOF. A program that prints forever
//!    turns a tool call into an out-of-memory kill of the whole kernel, and a program that
//!    prints a few megabytes turns one tool call into a model context nobody can afford.
//!    Both streams are therefore read concurrently into a fixed cap, and what is over the cap
//!    is discarded as it arrives rather than buffered and trimmed.
//! 2. **A pipe that is never drained.** Reading `stdout` to the end *before* touching
//!    `stderr` deadlocks the moment the child fills the `stderr` pipe buffer — the classic
//!    bug, and it needs a program that writes a few tens of kilobytes of diagnostics to
//!    trigger. So both are read at once, and `stdin` is written at the same time.
//! 3. **Survivors.** Killing the process handle kills one process. A program that forked is
//!    still running, still holding the pipes open, and a supervisor that then waits for EOF
//!    waits forever. The child is the leader of its own process group (see
//!    [`crate::limits`]), so a timeout signals the group, and the wait that follows is for a
//!    tree that is already dead.

use std::process::Stdio;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_core::{Error, Result};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::limits::{self, Limits};
use crate::sandbox::{Plan, Sandbox};

/// One stream a child produced, and whether it produced more than was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Captured {
    /// The bytes kept, decoded lossily.
    pub(crate) text: String,
    /// Whether the child wrote more than the cap and the rest was discarded.
    pub(crate) truncated: bool,
    /// Whether the bytes kept were not valid UTF-8 and replacement characters were
    /// substituted.
    ///
    /// Reported rather than hidden: a model told a command "succeeded" while its output was
    /// silently mangled will reason about text the program never wrote.
    pub(crate) lossy: bool,
}

/// What running one child produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completed {
    /// The exit status, absent when a signal ended the process instead.
    pub(crate) code: Option<i32>,
    /// The signal that ended the process, if one did.
    pub(crate) signal: Option<i32>,
    /// Standard output.
    pub(crate) stdout: Captured,
    /// Standard error.
    pub(crate) stderr: Captured,
    /// Whether the wall-clock budget for this call ran out and the tree was killed.
    pub(crate) timed_out: bool,
}

impl Completed {
    /// Whether the child ended the way a caller would call success.
    pub(crate) fn succeeded(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// Runs `plan`, enforcing `limits`, giving up after `budget`.
///
/// The two ways this returns `Err` are the two the *caller* has to react to rather than the
/// model: the execution context was cancelled, or the child could not be started at all.
/// Everything a program can do to itself — exiting non-zero, dying on a signal, running past
/// its own timeout, printing more than the cap — comes back as a [`Completed`], because each
/// of those is something a model can be told and can adjust for.
pub(crate) async fn run(
    sandbox: &Sandbox,
    plan: &Plan,
    limits: &Limits,
    stdin: Option<&str>,
    max_output_bytes: usize,
    budget: Duration,
    cx: &ExecutionContext,
) -> Result<Completed> {
    let mut command = sandbox.command(plan);
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            // Not an inherited descriptor. A child that shares this process's standard input
            // is a child that can read whatever the operator types next, including the answer
            // to the approval prompt that let it run.
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A dropped handle — a panic anywhere above, a cancelled task — must not leave the
        // child running. This covers the paths the explicit kill below does not reach.
        .kill_on_drop(true);
    apply_limits(&mut command, limits);

    let mut child = command
        .spawn()
        .map_err(|error| Error::wrap(format!("starting `{}`", plan.program.display()), error))?;

    // Captured before the first await: once the child has been reaped the handle no longer
    // knows its pid, and the pid is what the process group is named by.
    let pid = child.id();

    let mut input = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let write = async {
        if let (Some(handle), Some(text)) = (input.as_mut(), stdin) {
            // A child that exits without reading its input closes the pipe, and writing to a
            // closed pipe is that child's choice rather than this tool's failure.
            let _ = handle.write_all(text.as_bytes()).await;
            let _ = handle.shutdown().await;
        }
        drop(input);
    };
    let pumps = async {
        let (out, err) = tokio::join!(
            capture(stdout, max_output_bytes),
            capture(stderr, max_output_bytes),
        );
        (out, err)
    };

    let finished = async {
        let ((), (stdout, stderr)) = tokio::join!(write, pumps);
        let status = child.wait().await;
        (status, stdout, stderr)
    };

    tokio::pin!(finished);

    let mut timed_out = false;
    let outcome = tokio::select! {
        biased;
        () = cx.cancelled() => {
            terminate(pid).await;
            return Err(Error::Cancelled);
        }
        () = tokio::time::sleep(budget) => {
            timed_out = true;
            terminate(pid).await;
            // The pipes close once the tree is dead, so this is now a bounded wait for what
            // the child had already written rather than a wait on a live process.
            finished.await
        }
        outcome = &mut finished => outcome,
    };

    let (status, stdout, stderr) = outcome;
    let status = status.map_err(|error| Error::wrap("waiting for the child process", error))?;

    Ok(Completed {
        code: status.code(),
        signal: signal_of(&status),
        stdout,
        stderr,
        timed_out,
    })
}

/// Reads one stream up to `cap` bytes, discarding the rest as it arrives.
///
/// Discarding rather than accumulating is the point: a child that writes a gigabyte costs a
/// gigabyte of reads and a fixed amount of memory.
async fn capture<R>(stream: Option<R>, cap: usize) -> Captured
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut stream) = stream else {
        return Captured {
            text: String::new(),
            truncated: false,
            lossy: false,
        };
    };

    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                if room == 0 {
                    truncated = true;
                    continue;
                }
                if read > room {
                    truncated = true;
                }
                kept.extend_from_slice(&chunk[..read.min(room)]);
            }
            // A read error on a pipe whose writer was killed is the expected end of a
            // terminated child, not something a model can act on.
            Err(_) => break,
        }
    }

    match String::from_utf8(kept) {
        Ok(text) => Captured {
            text,
            truncated,
            lossy: false,
        },
        Err(invalid) => Captured {
            text: String::from_utf8_lossy(invalid.as_bytes()).into_owned(),
            truncated,
            lossy: true,
        },
    }
}

/// Kills the child's whole process group, if it is still identifiable.
async fn terminate(pid: Option<u32>) {
    if let Some(pid) = pid {
        limits::kill_group(pid);
    }
}

/// Installs the pre-exec hook that lowers the child's limits and gives it its own session.
#[cfg(unix)]
fn apply_limits(command: &mut tokio::process::Command, limits: &Limits) {
    use std::os::unix::process::CommandExt as _;

    let limits = *limits;
    // SAFETY: the closure runs in the forked child before `execve`, where only
    // async-signal-safe calls are permitted. `limits::apply` makes exactly two kinds of call —
    // `setrlimit` and `setsid` — and the `Limits` it reads is a `Copy` of plain integers moved
    // into the closure, so nothing is allocated, locked or shared with the parent.
    unsafe {
        command
            .as_std_mut()
            .pre_exec(move || limits::apply(&limits));
    }
}

/// See the Unix definition; there is no portable equivalent, and no
/// [`Sandbox`](crate::Sandbox) can be constructed off Unix to reach this.
#[cfg(not(unix))]
fn apply_limits(_command: &mut tokio::process::Command, _limits: &Limits) {}

/// The signal that ended a process, where the platform reports one.
#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// See the Unix definition.
#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_keeps_the_cap_and_reports_the_rest_as_discarded() {
        let source = std::io::Cursor::new(vec![b'x'; 100]);
        let captured = capture(Some(source), 10).await;

        assert_eq!(captured.text, "x".repeat(10));
        assert!(captured.truncated);
        assert!(!captured.lossy);
    }

    #[tokio::test]
    async fn capture_reports_output_that_was_not_text() {
        let source = std::io::Cursor::new(vec![0xff, 0xfe]);
        let captured = capture(Some(source), 64).await;

        assert!(captured.lossy);
        assert!(!captured.truncated);
    }

    #[tokio::test]
    async fn capture_of_output_that_fits_is_neither_truncated_nor_lossy() {
        let source = std::io::Cursor::new(b"ok\n".to_vec());
        let captured = capture(Some(source), 64).await;

        assert_eq!(captured.text, "ok\n");
        assert!(!captured.truncated);
        assert!(!captured.lossy);
    }
}
