//! Command-line parsing for `aikd`.
//!
//! Hand-written, like the terminal frontend's, and for the same reason: every option is a
//! string, a path or a flag. What a parser generator would add here is a dependency.
//!
//! # What is deliberately absent
//!
//! There is no `--daemonize`, no pid file and no logging configuration. A host process that
//! forks itself into the background is a process whose failures happen where nobody sees
//! them, and every service manager in use — systemd, launchd, a supervisor, a terminal —
//! prefers a process that stays in the foreground and writes to standard error. Backgrounding
//! is the caller's decision and the caller's mechanism.

use std::path::PathBuf;

use aik_core::{Error, Result};
use aik_runtime::{ExecSet, MemorySet};

/// The program name used in help and error messages.
pub const PROGRAM: &str = "aikd";

/// What the operator asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The socket to listen on, overriding `$AIK_SOCKET` and the default.
    pub socket: Option<PathBuf>,
    /// The model every turn is sent to, overriding configuration.
    pub model: Option<String>,
    /// The agent's identity, overriding configuration.
    pub agent: Option<String>,
    /// The user's identity, overriding configuration.
    pub user: Option<String>,
    /// The root the filesystem tools are confined to.
    pub root: Option<PathBuf>,
    /// A JSON configuration file.
    pub config: Option<PathBuf>,
    /// A JSON policy document, layered over whatever the configuration says.
    pub policy: Option<PathBuf>,
    /// Whether to register the filesystem write tool.
    pub write: bool,
    /// Whether to register no tools at all.
    pub no_tools: bool,
    /// Which memory tools to register, or `None` to take the default.
    pub memory: Option<MemorySet>,
    /// Whether to register the process-execution tool, and behind what.
    pub exec: Option<ExecSet>,
    /// Where the shared database lives.
    pub database: Option<PathBuf>,
    /// Whether to run with no database at all.
    pub ephemeral: bool,
    /// How many clients may be connected at once.
    pub max_connections: Option<usize>,
}

/// What parsing produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Serve with these options.
    Serve(Box<Options>),
    /// Print the help text and exit successfully.
    Help,
    /// Print the version and exit successfully.
    Version,
}

/// The help text, printed for `--help` and on a usage error.
pub const HELP: &str = concat!(
    "aikd — the AI kernel's host process\n",
    "\n",
    "USAGE:\n",
    "    aikd [OPTIONS]\n",
    "\n",
    "OPTIONS:\n",
    "    -s, --socket <PATH>  listen here [default: $AIK_SOCKET, else\n",
    "                         $XDG_RUNTIME_DIR/aik/aikd.sock]\n",
    "    -m, --model <ID>     model to use; defaults to configuration, then to the\n",
    "                         first model the provider reports\n",
    "    -a, --agent <ID>     the agent's identity, as policy sees it [default: assistant]\n",
    "    -u, --user <ID>      the user's identity, as policy sees it [default: user]\n",
    "    -r, --root <DIR>     directory the filesystem tools are confined to\n",
    "                         [default: the current directory]\n",
    "    -c, --config <FILE>  JSON configuration file\n",
    "    -p, --policy <FILE>  JSON policy document, overriding the one in --config\n",
    "        --write          also register the filesystem write tool\n",
    "        --no-tools       register no tools at all, memory included\n",
    "        --memory <MODE>  which memory tools to register: off, recall, remember, full\n",
    "        --exec <MODE>    run the programs in agent.exec.programs: off, sandboxed,\n",
    "                         unconfined (no sandbox — the allowlist is the only limit)\n",
    "                         [default: off]\n",
    "                         [default: remember]\n",
    "        --db <FILE>      the shared database file [default: the path in\n",
    "                         components.store.db.path, else\n",
    "                         $XDG_DATA_HOME/aik/aik.redb]\n",
    "        --ephemeral      open no database: nothing survives this process\n",
    "        --max-clients <N>  how many clients may be connected at once [default: 16]\n",
    "    -h, --help           print this help\n",
    "    -V, --version        print the version\n",
    "\n",
    "WHAT IT SERVES:\n",
    "    One kernel, one database, one schedule. Clients connect over a Unix socket in a\n",
    "    directory only this account can reach, present the token written beside it, and\n",
    "    ask for conversations, sessions, jobs and audit records. Every request runs as the\n",
    "    agent this host was configured with; a client cannot name a principal, a tool or a\n",
    "    policy, because there is nowhere in the protocol to put one.\n",
    "\n",
    "SCHEDULED WORK:\n",
    "    This is the process that runs it. A firing acts as `scheduler` on behalf of the\n",
    "    job's owner, every tool call it makes is still gated by policy, and an approval\n",
    "    with no client attached to answer it is refused rather than granted.\n",
    "\n",
    "SHUTDOWN:\n",
    "    SIGINT or SIGTERM. Clients are told, work in flight is cancelled, parked approvals\n",
    "    are refused, and the socket and its token are removed.\n",
);

/// Parses command-line arguments, excluding the program name.
pub fn parse<I, S>(args: I) -> Result<Invocation>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut options = Options::default();
    let mut rest = args.into_iter().map(Into::into).collect::<Vec<_>>();
    rest.reverse();

    while let Some(argument) = rest.pop() {
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) if flag.starts_with('-') => (flag.to_owned(), Some(value)),
            _ => (argument.clone(), None),
        };

        let mut value = |name: &str| -> Result<String> {
            match inline {
                Some(value) => Ok(value.to_owned()),
                None => rest
                    .pop()
                    .ok_or_else(|| usage(format!("`{name}` needs a value"))),
            }
        };

        match flag.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "-s" | "--socket" => options.socket = Some(PathBuf::from(value(&flag)?)),
            "-m" | "--model" => options.model = Some(value(&flag)?),
            "-a" | "--agent" => options.agent = Some(value(&flag)?),
            "-u" | "--user" => options.user = Some(value(&flag)?),
            "-r" | "--root" => options.root = Some(PathBuf::from(value(&flag)?)),
            "-c" | "--config" => options.config = Some(PathBuf::from(value(&flag)?)),
            "-p" | "--policy" => options.policy = Some(PathBuf::from(value(&flag)?)),
            "--write" => options.write = true,
            "--no-tools" => options.no_tools = true,
            "--exec" => {
                options.exec = Some(
                    ExecSet::parse(&value(&flag)?)
                        .map_err(|error| usage(format!("`--exec` is wrong: {error}")))?,
                );
            }
            "--memory" => {
                let raw = value(&flag)?;
                options.memory = Some(
                    MemorySet::parse(&raw)
                        .map_err(|error| usage(format!("`--memory` is wrong: {error}")))?,
                );
            }
            "--db" => options.database = Some(PathBuf::from(value(&flag)?)),
            "--ephemeral" => options.ephemeral = true,
            "--max-clients" => options.max_connections = Some(parse_count(&value(&flag)?)?),
            other => return Err(usage(format!("unknown option `{other}`"))),
        }
    }

    if options.no_tools && options.write {
        return Err(usage(
            "`--no-tools` and `--write` contradict each other".to_owned(),
        ));
    }
    if options.no_tools && options.memory.is_some() {
        return Err(usage(
            "`--no-tools` and `--memory` contradict each other".to_owned(),
        ));
    }

    if options.no_tools && options.exec.is_some() {
        return Err(usage(
            "`--no-tools` and `--exec` contradict each other".to_owned(),
        ));
    }
    if options.ephemeral && options.database.is_some() {
        return Err(usage(
            "`--ephemeral` and `--db` contradict each other".to_owned(),
        ));
    }

    Ok(Invocation::Serve(Box::new(options)))
}

/// Which filesystem tools these options ask for.
impl Options {
    /// The tool set these options select.
    pub fn tools(&self) -> aik_runtime::ToolSet {
        use aik_runtime::ToolSet;
        match (self.no_tools, self.write) {
            (true, _) => ToolSet::None,
            (false, true) => ToolSet::ReadWrite,
            (false, false) => ToolSet::ReadOnly,
        }
    }

    /// The memory tools these options select.
    ///
    /// `--no-tools` means no tools, and has to keep meaning that as tools are added, so it
    /// overrides the memory default rather than leaving the record store reachable.
    pub fn memory(&self) -> MemorySet {
        if self.no_tools {
            return MemorySet::Off;
        }
        self.memory.unwrap_or_default()
    }

    /// Whether these options ask for the process-execution tool. See [`Options::memory`].
    pub fn exec(&self) -> ExecSet {
        if self.no_tools {
            return ExecSet::Off;
        }
        self.exec.unwrap_or_default()
    }
}

fn parse_count(raw: &str) -> Result<usize> {
    match raw.parse::<usize>() {
        Ok(0) => Err(usage(
            "`--max-clients` must be at least 1; a host that serves nobody is a host that is \
             not running"
                .to_owned(),
        )),
        Ok(count) => Ok(count),
        Err(_) => Err(usage(format!(
            "`--max-clients` takes a number of clients; `{raw}` is not one"
        ))),
    }
}

fn usage(message: String) -> Error {
    Error::InvalidArgument(format!("{message}\n\nRun `{PROGRAM} --help` for usage."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(args: &[&str]) -> Options {
        match parse(args.iter().map(|argument| (*argument).to_owned())).expect("parsed") {
            Invocation::Serve(options) => *options,
            other => panic!("expected options, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_serves_with_read_only_tools() {
        let options = serve(&[]);
        assert_eq!(options.tools(), aik_runtime::ToolSet::ReadOnly);
        assert_eq!(options.memory(), MemorySet::Remember);
        assert_eq!(options.socket, None);
    }

    #[test]
    fn no_tools_means_no_memory_either() {
        let options = serve(&["--no-tools"]);
        assert_eq!(options.tools(), aik_runtime::ToolSet::None);
        assert_eq!(options.memory(), MemorySet::Off);
        assert_eq!(options.exec(), ExecSet::Off);
    }

    #[test]
    fn a_host_runs_no_programs_unless_asked_and_names_the_modes_it_takes() {
        assert_eq!(serve(&[]).exec(), ExecSet::Off);
        assert_eq!(serve(&["--exec", "sandboxed"]).exec(), ExecSet::Sandboxed);

        let error = parse(["--exec", "sandbox"].iter().map(|a| (*a).to_owned())).unwrap_err();
        assert!(
            format!("{error}").contains("off, sandboxed, unconfined"),
            "{error}"
        );
    }

    #[test]
    fn values_are_accepted_both_ways_round() {
        assert_eq!(
            serve(&["--socket", "/tmp/a.sock"]).socket,
            serve(&["--socket=/tmp/a.sock"]).socket,
        );
    }

    #[test]
    fn contradictions_are_refused_rather_than_resolved() {
        for arguments in [
            vec!["--no-tools", "--write"],
            vec!["--no-tools", "--memory", "full"],
            vec!["--ephemeral", "--db", "/tmp/a.redb"],
        ] {
            let error = parse(arguments.iter().map(|a| (*a).to_owned()))
                .expect_err("a contradiction must not be silently resolved");
            assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
        }
    }

    #[test]
    fn an_unknown_option_is_an_error_rather_than_being_ignored() {
        let error = parse(["--wirte".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("--wirte"), "{error}");
    }

    #[test]
    fn a_host_that_serves_nobody_is_refused() {
        let error = parse(["--max-clients=0".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("at least 1"), "{error}");
    }

    #[test]
    fn help_and_version_win_over_everything_after_them() {
        assert_eq!(
            parse(["--help".to_owned()]).expect("parsed"),
            Invocation::Help
        );
        assert_eq!(
            parse(["-V".to_owned(), "--nonsense".to_owned()]).expect("parsed"),
            Invocation::Version,
        );
    }
}
