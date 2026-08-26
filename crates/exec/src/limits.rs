//! Resource limits, and the session the child is put in before it is executed.
//!
//! These apply to *every* run, sandboxed or not. A namespace sandbox bounds what a child can
//! reach; it does not bound what a child can consume, and a process that spins on the CPU or
//! fills a disk inside its own mount namespace has still taken the machine down. The two
//! mechanisms are independent and both are always on.
//!
//! Everything here runs between `fork` and `execve`, so it is written to the rules that apply
//! there: no allocation, no locks, no Rust runtime services, only async-signal-safe calls.

use std::time::Duration;

/// How much of each resource one run may consume.
///
/// `None` means "leave whatever the parent had", which is only ever the right answer for the
/// two limits that are unsafe to guess at — see [`Limits::address_space_bytes`] and
/// [`Limits::processes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// CPU seconds, after which the kernel sends `SIGXCPU` and then `SIGKILL`.
    ///
    /// This is the limit that survives everything else. A wall-clock timeout depends on this
    /// process still being there to enforce it; `RLIMIT_CPU` is enforced by the OS whatever
    /// happens here, so a runaway child dies even if the kernel that started it does not.
    pub cpu_seconds: Option<u64>,
    /// The largest file the child may create, in bytes.
    ///
    /// Bounds the other way a confined process can still hurt a host: filling the filesystem
    /// its workspace lives on. Exceeding it raises `SIGXFSZ`, which is fatal by default.
    pub file_size_bytes: Option<u64>,
    /// The highest file descriptor number the child may open.
    pub open_files: Option<u64>,
    /// Total address space, in bytes.
    ///
    /// `None` by default and deliberately so: several language runtimes reserve enormous
    /// virtual mappings they never fault in, so a limit that sounds generous makes them fail
    /// to start at all. A deployment that knows what it runs can set one; a default that
    /// broke `python3` would be a limit nobody kept.
    pub address_space_bytes: Option<u64>,
    /// Processes per real user id.
    ///
    /// `None` by default, because `RLIMIT_NPROC` counts every process the *host account*
    /// already has, not just this child's. A value low enough to stop a fork bomb is low
    /// enough that a busy desktop session fails the very first call, and a value high enough
    /// to be safe from that stops nothing. Containment of runaway forking comes from the
    /// sandbox's pid namespace and from [`Limits::cpu_seconds`] instead.
    pub processes: Option<u64>,
}

impl Limits {
    /// The limits applied when a deployment configures none.
    ///
    /// Both defaults are sized to be invisible to legitimate work and fatal to a runaway one.
    pub const DEFAULT_FILE_SIZE_BYTES: u64 = 64 * 1024 * 1024;
    /// See [`Limits::open_files`].
    pub const DEFAULT_OPEN_FILES: u64 = 256;

    /// Limits derived from the wall-clock budget one call is allowed.
    ///
    /// The CPU limit is the wall-clock budget rounded up, plus a second of slack, so that the
    /// wall-clock timeout is what normally fires — it produces a clean, model-readable
    /// "timed out" — and `RLIMIT_CPU` is only reached by a child that outlived the process
    /// supervising it.
    pub fn for_budget(budget: Duration) -> Self {
        Self {
            cpu_seconds: Some(budget.as_secs().saturating_add(2)),
            file_size_bytes: Some(Self::DEFAULT_FILE_SIZE_BYTES),
            open_files: Some(Self::DEFAULT_OPEN_FILES),
            address_space_bytes: None,
            processes: None,
        }
    }
}

/// How `libc` types the first argument of `getrlimit`, which differs between C libraries.
#[cfg(all(unix, target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
/// See the `gnu` definition above.
#[cfg(all(unix, not(target_env = "gnu")))]
type Resource = libc::c_int;

/// Applies `limits` to the calling process, and detaches it from any controlling terminal.
///
/// Called from a `pre_exec` hook, so this runs in the forked child, before `execve`.
///
/// # The session matters as much as the limits
///
/// `setsid` does two things that are load-bearing:
///
/// * It gives the child a session and a process group of its own, so a timeout can kill the
///   whole tree with one `killpg` rather than the one process this code happens to hold a
///   handle to. A child that forks and exits would otherwise leave its descendants running.
/// * It drops the controlling terminal. A process that keeps the terminal can push bytes back
///   into its input queue with `TIOCSTI`, which on a frontend that is about to display an
///   approval prompt is a way to answer that prompt. There is no argument for a tool child
///   ever holding the operator's terminal.
///
/// # Safety
///
/// Every call made here is async-signal-safe, which is what `pre_exec` requires: `setrlimit`
/// and `setsid` are plain syscalls, and nothing between them allocates, takes a lock, or
/// touches memory shared with the parent.
#[cfg(unix)]
pub fn apply(limits: &Limits) -> std::io::Result<()> {
    // Errors are reported rather than ignored: a limit that silently failed to apply is a
    // limit a deployment believes it has. `pre_exec` turns this `Err` into a spawn failure,
    // so the call fails closed instead of running unbounded.
    set(libc::RLIMIT_CPU, limits.cpu_seconds)?;
    set(libc::RLIMIT_FSIZE, limits.file_size_bytes)?;
    set(libc::RLIMIT_NOFILE, limits.open_files)?;
    set(libc::RLIMIT_AS, limits.address_space_bytes)?;
    set(libc::RLIMIT_NPROC, limits.processes)?;
    // A core dump of a sandboxed child would be written outside the sandbox, by the host
    // kernel, containing whatever the child had read. Never useful here, so never produced.
    set(libc::RLIMIT_CORE, Some(0))?;

    // SAFETY: `setsid` is async-signal-safe and takes no arguments. It fails only when the
    // caller is already a process group leader, which a freshly forked child never is.
    if unsafe { libc::setsid() } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Lowers one limit, leaving it alone when the deployment configured none.
///
/// Only ever lowers: the hard limit is set to the same value as the soft one, so nothing the
/// child does can raise it back, and a parent whose own limit is already lower stays lower.
#[cfg(unix)]
fn set(resource: Resource, value: Option<u64>) -> std::io::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };

    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into a `rlimit` this frame owns, for a resource that is a
    // compile-time constant.
    if unsafe { libc::getrlimit(resource, &mut current) } < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let requested = value as libc::rlim_t;
    let ceiling = if current.rlim_max == libc::RLIM_INFINITY {
        requested
    } else {
        requested.min(current.rlim_max)
    };
    let limit = libc::rlimit {
        rlim_cur: ceiling,
        rlim_max: ceiling,
    };
    // SAFETY: as above, reading a `rlimit` this frame owns.
    if unsafe { libc::setrlimit(resource, &limit) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Kills a process group, and everything in it.
///
/// `pid` is the child's, which [`apply`] made the leader of its own group, so the negation is
/// the whole tree the child started. A group that has already exited is not an error — the
/// race between a timeout firing and a process finishing on its own is expected and common.
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    // SAFETY: `kill` with a negative pid signals a process group and cannot affect memory.
    // An `ESRCH` from a group that is already gone is the expected race, not a failure.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// The non-Unix stand-in for [`apply`].
///
/// Unreachable: a tool is only ever constructed on a platform whose runner exists, and
/// [`Runner`](crate::runner::Runner) does not exist off Unix. Present so the crate compiles
/// everywhere the workspace does rather than being conditionally absent.
#[cfg(not(unix))]
pub fn apply(_limits: &Limits) -> std::io::Result<()> {
    Ok(())
}

/// The non-Unix stand-in for [`kill_group`]. See [`apply`].
#[cfg(not(unix))]
pub fn kill_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_leaves_cpu_slack_over_the_wall_clock() {
        let limits = Limits::for_budget(Duration::from_millis(1500));
        assert_eq!(limits.cpu_seconds, Some(3));
    }

    #[test]
    fn the_unsafe_to_guess_limits_are_unset_by_default() {
        let limits = Limits::for_budget(Duration::from_secs(30));
        assert_eq!(limits.address_space_bytes, None);
        assert_eq!(limits.processes, None);
    }
}
