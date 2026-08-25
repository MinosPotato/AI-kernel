//! The enforcement boundary a child process runs behind.
//!
//! Everything else in this crate is a *cooperative* check: a name validated, an allowlist
//! consulted, an environment scrubbed, a limit lowered. Cooperative checks bound what a
//! correct program does with the arguments it was given. They bound nothing at all about what
//! the program then chooses to do — a program is not a tool implementation, it is arbitrary
//! host code, and the first thing many useful programs do is open files nobody named.
//!
//! So this is the one part of `aik-exec` that is not advice. A [`Sandbox::Bubblewrap`] child
//! runs in its own user, mount, pid, ipc and uts namespaces, with a read-only view of the
//! system directories it needs to load, no network unless the deployment granted one, a
//! tmpfs for `/tmp` and exactly one writable path: the workspace.
//!
//! # Why bubblewrap rather than namespaces directly
//!
//! Setting this up by hand means `unshare(2)`, writing `uid_map`/`gid_map` in the right order
//! with `setgroups` denied first, pivoting root, and building a mount tree — all between
//! `fork` and `execve`, where a mistake is a silently weaker sandbox rather than a compile
//! error. `bwrap` is the widely deployed, audited program that already does exactly that, and
//! it is what Flatpak confines applications with. Reimplementing it here would add several
//! hundred lines of `unsafe` whose failure mode is "the boundary quietly is not there".
//!
//! The cost is a runtime dependency, and it is paid at construction rather than at the first
//! call: [`Sandbox::bubblewrap`] finds the binary and then *proves it works* by starting a
//! throwaway sandbox. A deployment on a host where user namespaces are disabled fails to
//! start, with the reason, instead of appearing to run confined.
//!
//! # What this is not
//!
//! No syscall filter is installed. A sandboxed child can make any system call the host kernel
//! offers, and is separated from the host by namespaces and mount visibility rather than by a
//! seccomp policy. That is a real limit: it means the boundary is only as strong as the
//! kernel's user-namespace implementation, and a local kernel privilege escalation defeats it.
//! It is not a limit that a seccomp filter written here would remove, because the useful
//! programs this exists to run need a broad syscall surface anyway.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use aik_core::{Error, Result};

use crate::program;

/// Where the workspace appears inside a sandboxed child's filesystem.
///
/// A fixed path rather than the host's, so nothing about where the deployment keeps its files
/// — a user account name, a project layout — reaches the child or the output it produces.
pub const WORKSPACE: &str = "/workspace";

/// The search path [`Sandbox::bubblewrap`] looks for `bwrap` on.
///
/// Fixed rather than taken from the environment, for the same reason a program is never
/// looked up on it either: the
/// process `PATH` is inherited from whoever started the kernel, and a `bwrap` found on it
/// could be anything. A deployment that keeps bubblewrap elsewhere names the binary itself
/// with [`Sandbox::bubblewrap_at`].
pub const SANDBOX_SEARCH_PATH: &str = "/usr/bin:/bin:/usr/local/bin";

/// The system directories a sandboxed child gets a read-only view of.
///
/// Enough to load a dynamically linked program and nothing else. `/etc` is *not* here: it
/// holds `passwd`, `shadow`, ssh host keys and most of a host's configuration, and a program
/// that genuinely needs one file from it should be granted that file, not the directory.
const READ_ONLY_ROOTS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64"];

/// The individual `/etc` files a dynamic loader consults.
///
/// Named one by one, deliberately. See [`READ_ONLY_ROOTS`].
const READ_ONLY_FILES: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
    "/etc/localtime",
];

/// What a networked child additionally needs to resolve a name and verify a certificate.
///
/// Bound only when the deployment granted network access, so a no-network sandbox cannot even
/// see which resolver or which certificate authorities the host trusts.
const NETWORK_FILES: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
];

/// How a child is separated from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sandbox {
    /// A bubblewrap-backed namespace sandbox, using the binary at this path.
    ///
    /// The only variant that provides an enforcement boundary. Constructed through
    /// [`Sandbox::bubblewrap`] or [`Sandbox::bubblewrap_at`], both of which verify the
    /// sandbox actually starts before returning.
    Bubblewrap(PathBuf),
    /// No sandbox at all.
    ///
    /// The child runs as the account the kernel runs as, seeing the whole host filesystem
    /// with that account's privileges. The other measures still apply — an allowlisted
    /// program, an argument vector rather than a shell string, a scrubbed environment,
    /// resource limits, a session of its own — but **none of them confines what the program
    /// does once it is running**. `cat` allowlisted here reads anything the host account can
    /// read, whatever the workspace is set to.
    ///
    /// It exists because there are hosts where no sandbox is available and an operator has
    /// decided, knowingly, that the program allowlist is the boundary they want. It is never
    /// a default, never reached by omitting configuration, and a deployment that selects it
    /// should treat the allowlist as the entire security argument.
    Unconfined,
}

impl Sandbox {
    /// Finds `bwrap` on [`SANDBOX_SEARCH_PATH`] and verifies that it can start a sandbox.
    pub fn bubblewrap() -> Result<Self> {
        let search = program::parse_search_path(SANDBOX_SEARCH_PATH);
        let binary = program::resolve("bwrap", &search).map_err(|_| {
            Error::config(
                "aik-exec.sandbox",
                format!(
                    "no `bwrap` on `{SANDBOX_SEARCH_PATH}`; install bubblewrap, or name it \
                     explicitly, or configure an unconfined deployment knowing what that means"
                ),
            )
        })?;
        Self::bubblewrap_at(binary)
    }

    /// Uses the `bwrap` at `binary`, after verifying that it can start a sandbox.
    pub fn bubblewrap_at(binary: impl Into<PathBuf>) -> Result<Self> {
        let sandbox = Self::Bubblewrap(binary.into());
        sandbox.probe()?;
        Ok(sandbox)
    }

    /// The unconfined mode, having read what [`Sandbox::Unconfined`] says it is.
    ///
    /// A named constructor rather than a bare variant so that selecting it is a sentence
    /// somebody wrote, greppable across a deployment, rather than a value that could be
    /// arrived at by a `Default` nobody looked at.
    pub fn unconfined() -> Self {
        Self::Unconfined
    }

    /// Whether this mode actually confines anything.
    pub fn is_enforcing(&self) -> bool {
        matches!(self, Self::Bubblewrap(_))
    }

    /// Starts a throwaway sandbox to check the host supports one.
    ///
    /// The probe builds the *same* plan a real call would — every namespace, the whole mount
    /// tree, a workspace bound read-only — and then runs a program inside it that must succeed
    /// if, and only if, all of that worked.
    ///
    /// The program is `bwrap` itself, asked for its version. Using the binary this crate
    /// already depends on is what keeps the check honest: it adds no assumption about what
    /// else the host has installed, it is dynamically linked like anything else worth running
    /// and so exercises the loader's view of the mount tree, and its output is unmistakable.
    /// An earlier version of this probe instead ran a path that could not exist and treated
    /// "command not found" as success — which is not distinguishable, by exit status or by
    /// message, from a sandbox bubblewrap failed to build. That probe reported failure on a
    /// host where the sandbox worked perfectly, and would have reported success on one where a
    /// bind silently did nothing.
    fn probe(&self) -> Result<()> {
        let Self::Bubblewrap(binary) = self else {
            return Ok(());
        };

        // `/usr` stands in for the workspace: it is bound read-only, so the probe writes
        // nothing anywhere, and it is the one directory a host that can run programs at all is
        // certain to have. Creating a temporary directory instead would make a startup check
        // into a filesystem write.
        let plan = Plan {
            program: binary.clone(),
            arguments: vec!["--version".to_owned()],
            workspace: PathBuf::from("/usr"),
            writable: false,
            network: false,
            environment: Vec::new(),
            tmpfs_bytes: DEFAULT_TMPFS_BYTES,
        };

        let mut command = std::process::Command::new(binary);
        command.args(self.namespace_arguments(&plan));
        command.args(self.filesystem_arguments(&plan));
        // Bound explicitly so the probe works wherever the binary lives, including outside the
        // read-only roots the mount plan covers. Redundant when it is already under `/usr`, and
        // binding a path over itself is what bubblewrap does anyway.
        command.arg("--ro-bind");
        command.arg(binary);
        command.arg(binary);
        command.arg("--");
        command.arg(binary);
        command.arg("--version");
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let output = command.output().map_err(|error| {
            Error::config(
                "aik-exec.sandbox",
                format!("cannot run `{}`: {error}", binary.display()),
            )
        })?;

        let printed = String::from_utf8_lossy(&output.stdout);
        if output.status.success() && printed.starts_with("bubblewrap") {
            return Ok(());
        }
        Err(Error::config(
            "aik-exec.sandbox",
            format!(
                "`{}` cannot start a sandbox on this host ({}): {}",
                binary.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            ),
        ))
    }

    /// The namespace, lifetime and capability flags, which never depend on the filesystem.
    fn namespace_arguments(&self, plan: &Plan) -> Vec<OsString> {
        let mut arguments: Vec<OsString> = Vec::new();
        let mut push = |flag: &str| arguments.push(OsString::from(flag));

        // Not the `-try` forms. A host that cannot give the child its own user namespace
        // cannot confine it, and a sandbox that quietly downgraded to "no separation" is
        // worse than one that refuses to start.
        push("--unshare-user");
        push("--unshare-ipc");
        push("--unshare-pid");
        push("--unshare-uts");
        // The exception, because cgroup namespaces postdate the others by years and their
        // absence weakens nothing else here.
        push("--unshare-cgroup-try");
        if !plan.network {
            push("--unshare-net");
        }

        // The host's name is not the child's business, and a program that prints it should
        // print something that says where it ran.
        push("--hostname");
        push("aik-sandbox");

        // Two independent guarantees that a child outlives nothing: the kernel kills it when
        // this process dies, and it is in a session of its own with no terminal to reach back
        // through. `--new-session` is what makes `TIOCSTI` against the operator's terminal
        // impossible rather than merely unlikely.
        push("--die-with-parent");
        push("--new-session");
        push("--cap-drop");
        push("ALL");

        // Nothing of this process's environment reaches the child. Whatever it needs is set
        // back, by name, from the plan.
        push("--clearenv");
        for (key, value) in &plan.environment {
            arguments.push(OsString::from("--setenv"));
            arguments.push(OsString::from(key));
            arguments.push(OsString::from(value));
        }
        arguments
    }

    /// The mount plan: what the child can see, and the one thing it can write.
    fn filesystem_arguments(&self, plan: &Plan) -> Vec<OsString> {
        let mut arguments: Vec<OsString> = Vec::new();
        let flag = |arguments: &mut Vec<OsString>, values: &[&str]| {
            arguments.extend(values.iter().map(OsString::from));
        };

        for root in READ_ONLY_ROOTS {
            flag(&mut arguments, &["--ro-bind-try", root, root]);
        }
        for file in READ_ONLY_FILES {
            flag(&mut arguments, &["--ro-bind-try", file, file]);
        }
        if plan.network {
            for file in NETWORK_FILES {
                flag(&mut arguments, &["--ro-bind-try", file, file]);
            }
        }

        flag(&mut arguments, &["--proc", "/proc"]);
        flag(&mut arguments, &["--dev", "/dev"]);
        // Bounded, because a tmpfs is memory: an unbounded `/tmp` inside the sandbox is a way
        // to exhaust the host's RAM that no rlimit on the child would catch.
        flag(&mut arguments, &["--perms", "01777"]);
        arguments.push(OsString::from("--size"));
        arguments.push(OsString::from(plan.tmpfs_bytes.to_string()));
        flag(&mut arguments, &["--tmpfs", "/tmp"]);

        // Last, so that nothing above can be layered over the workspace, and so that the one
        // writable path in the whole sandbox is unmistakable when these arguments are read.
        arguments.push(OsString::from(if plan.writable {
            "--bind"
        } else {
            "--ro-bind"
        }));
        arguments.push(plan.workspace.clone().into_os_string());
        arguments.push(OsString::from(WORKSPACE));
        flag(&mut arguments, &["--chdir", WORKSPACE]);
        arguments
    }

    /// Builds the command that runs `plan`, sandboxed or not.
    pub(crate) fn command(&self, plan: &Plan) -> tokio::process::Command {
        match self {
            Self::Bubblewrap(binary) => {
                let mut command = tokio::process::Command::new(binary);
                command.args(self.namespace_arguments(plan));
                command.args(self.filesystem_arguments(plan));
                command.arg("--");
                command.arg(&plan.program);
                command.args(&plan.arguments);
                command
            }
            Self::Unconfined => {
                let mut command = tokio::process::Command::new(&plan.program);
                command.args(&plan.arguments);
                command.current_dir(&plan.workspace);
                // The same scrubbing the sandboxed path gets from `--clearenv`, done here
                // because there is no sandbox to do it. This is the measure that keeps the
                // kernel's own configuration — endpoints, database paths, anything a host
                // exported before starting it — out of a child's environment.
                command.env_clear();
                for (key, value) in &plan.environment {
                    command.env(key, value);
                }
                command
            }
        }
    }

    /// The path the child sees the workspace at, which the two modes disagree about.
    pub(crate) fn workspace_as_seen(&self, workspace: &Path) -> String {
        match self {
            Self::Bubblewrap(_) => WORKSPACE.to_owned(),
            Self::Unconfined => program::display(workspace.as_os_str()),
        }
    }
}

/// The default size of the sandbox's `/tmp`.
pub const DEFAULT_TMPFS_BYTES: u64 = 64 * 1024 * 1024;

/// One resolved run, with every decision about it already made.
///
/// Built only by [`ExecTool`](crate::ExecTool), after the program name has been validated,
/// checked against the allowlist and resolved to an absolute path. Nothing here is still a
/// request; every field is something the tool decided.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    /// The absolute path of the program to execute.
    pub(crate) program: PathBuf,
    /// Its arguments, exactly as they will be passed. Never a shell string.
    pub(crate) arguments: Vec<String>,
    /// The host directory the child works in, and the only path it may write.
    pub(crate) workspace: PathBuf,
    /// Whether the workspace is writable.
    pub(crate) writable: bool,
    /// Whether the child has a network.
    pub(crate) network: bool,
    /// The child's entire environment, in order.
    pub(crate) environment: Vec<(String, String)>,
    /// The size of the sandbox's `/tmp`.
    pub(crate) tmpfs_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            program: PathBuf::from("/usr/bin/git"),
            arguments: vec!["status".to_owned()],
            workspace: PathBuf::from("/home/somebody/project"),
            writable: false,
            network: false,
            environment: vec![("PATH".to_owned(), "/usr/bin".to_owned())],
            tmpfs_bytes: DEFAULT_TMPFS_BYTES,
        }
    }

    fn rendered(sandbox: &Sandbox, plan: &Plan) -> Vec<String> {
        sandbox
            .namespace_arguments(plan)
            .into_iter()
            .chain(sandbox.filesystem_arguments(plan))
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_sandbox_never_shares_a_namespace_it_was_not_asked_to() {
        let sandbox = Sandbox::Bubblewrap(PathBuf::from("/usr/bin/bwrap"));
        let arguments = rendered(&sandbox, &plan());

        for flag in [
            "--unshare-user",
            "--unshare-ipc",
            "--unshare-pid",
            "--unshare-net",
        ] {
            assert!(arguments.contains(&flag.to_owned()), "missing {flag}");
        }
    }

    #[test]
    fn granting_a_network_is_the_only_thing_that_shares_one() {
        let sandbox = Sandbox::Bubblewrap(PathBuf::from("/usr/bin/bwrap"));
        let mut networked = plan();
        networked.network = true;

        let arguments = rendered(&sandbox, &networked);
        assert!(!arguments.contains(&"--unshare-net".to_owned()));
        assert!(arguments.contains(&"/etc/resolv.conf".to_owned()));

        let confined = rendered(&sandbox, &plan());
        assert!(!confined.contains(&"/etc/resolv.conf".to_owned()));
    }

    #[test]
    fn the_workspace_is_the_only_writable_mount_and_only_when_asked() {
        let sandbox = Sandbox::Bubblewrap(PathBuf::from("/usr/bin/bwrap"));

        let read_only = rendered(&sandbox, &plan());
        assert!(!read_only.contains(&"--bind".to_owned()));

        let mut writable = plan();
        writable.writable = true;
        let arguments = rendered(&sandbox, &writable);
        let binds: Vec<_> = arguments
            .iter()
            .enumerate()
            .filter(|(_, argument)| argument.as_str() == "--bind")
            .collect();
        assert_eq!(binds.len(), 1);
        assert_eq!(arguments[binds[0].0 + 1], "/home/somebody/project");
        assert_eq!(arguments[binds[0].0 + 2], WORKSPACE);
    }

    #[test]
    fn nothing_of_this_process_environment_is_carried_in() {
        let sandbox = Sandbox::Bubblewrap(PathBuf::from("/usr/bin/bwrap"));
        let arguments = rendered(&sandbox, &plan());
        assert!(arguments.contains(&"--clearenv".to_owned()));

        let index = arguments.iter().position(|a| a == "--clearenv").unwrap();
        assert_eq!(arguments[index + 1], "--setenv");
        assert_eq!(arguments[index + 2], "PATH");
    }

    #[test]
    fn etc_is_never_bound_wholesale() {
        let sandbox = Sandbox::Bubblewrap(PathBuf::from("/usr/bin/bwrap"));
        let mut networked = plan();
        networked.network = true;
        for arguments in [rendered(&sandbox, &plan()), rendered(&sandbox, &networked)] {
            assert!(!arguments.contains(&"/etc".to_owned()));
            assert!(
                !arguments
                    .iter()
                    .any(|a| a == "/etc/passwd" || a == "/etc/shadow")
            );
        }
    }

    #[test]
    fn the_probe_accepts_a_host_that_can_actually_sandbox() {
        // The regression this exists for: a probe whose success condition was wrong reported
        // failure on a working host, which turned every test that needed a sandbox into a
        // silent skip. It has to be checked here, against the real binary, because a probe is
        // exactly the thing a mock cannot stand in for.
        if !Path::new("/usr/bin/bwrap").exists() {
            eprintln!("skipped: this host has no bubblewrap");
            return;
        }
        Sandbox::bubblewrap().expect("bubblewrap is installed and should start a sandbox");
    }

    #[test]
    fn the_probe_refuses_something_that_is_not_bubblewrap() {
        // `true` starts, exits 0, and prints nothing. A probe that only looked at the exit
        // status would accept it and report a sandbox that does not exist.
        let Ok(binary) = std::fs::canonicalize("/usr/bin/true") else {
            eprintln!("skipped: this host has no /usr/bin/true");
            return;
        };
        let error = Sandbox::bubblewrap_at(binary).expect_err("that is not a sandbox");
        assert_eq!(error.kind(), aik_core::ErrorKind::Config);
    }

    #[test]
    fn an_unconfined_run_still_clears_the_environment_and_stays_in_the_workspace() {
        let sandbox = Sandbox::Unconfined;
        let command = sandbox.command(&plan());
        let inner = command.as_std();

        assert_eq!(
            inner.get_current_dir(),
            Some(Path::new("/home/somebody/project"))
        );
        let environment: Vec<_> = inner.get_envs().collect();
        assert_eq!(environment.len(), 1);
        assert_eq!(environment[0].0, "PATH");
    }
}
