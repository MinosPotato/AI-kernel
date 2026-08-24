//! Command-line parsing.
//!
//! Hand-written rather than derived from a dependency: every option is a string, a path or a
//! flag, and the whole grammar fits on a screen. What a parser generator would add here is a
//! dependency, not a capability.

use std::path::PathBuf;
use std::time::Duration;

use aik_api::agent::SessionId;
use aik_api::audit::AuditEntryKind;
use aik_core::id::CorrelationId;
use aik_core::{Error, Result};

/// The program name used in help and error messages.
pub const PROGRAM: &str = "aik";

/// Which filesystem tools a run registers, and which memory tools.
///
/// Re-exported from [`aik_runtime`] rather than defined here: both are *wiring* decisions —
/// which capability exists at all — and the host process makes exactly the same two. A
/// second definition would be a second thing to keep in step, and the drift would show up as
/// a tool present in one frontend and absent in the other.
pub use aik_runtime::{MemorySet, ToolSet};

/// What the user asked for on the command line.
///
/// [`Default`] is the no-arguments invocation: an interactive session, read-only tools, and
/// everything else left to configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// The one-shot prompt, or `None` for an interactive session.
    pub prompt: Option<String>,
    /// A durable session to resume, or `None` to start a new one.
    ///
    /// Parsed as a [`SessionId`] here so a typo is a usage error
    /// before a kernel is built, rather than a lookup that finds nothing much later. Whether
    /// the session *exists*, and whether this run may touch it, are the store's questions and
    /// are deliberately not asked here — see [`crate::session`].
    pub session: Option<SessionId>,
    /// The model to send every turn to, overriding configuration.
    pub model: Option<String>,
    /// The agent's identity, overriding configuration.
    pub agent: Option<String>,
    /// The user's identity, overriding configuration.
    pub user: Option<String>,
    /// The root the filesystem tools are confined to, overriding configuration.
    pub root: Option<PathBuf>,
    /// A JSON configuration file to layer under the environment.
    pub config: Option<PathBuf>,
    /// A JSON policy document, layered over whatever the configuration says.
    pub policy: Option<PathBuf>,
    /// Whether to register the filesystem write tool.
    pub write: bool,
    /// Whether to register no tools at all.
    pub no_tools: bool,
    /// Which memory tools to register, or `None` to take the default.
    ///
    /// An `Option` rather than a bare [`MemorySet`] so that `--memory` combined with
    /// `--no-tools` can be rejected as the contradiction it is, instead of one silently
    /// winning.
    pub memory: Option<MemorySet>,
    /// Where the shared database lives, overriding configuration and the XDG default.
    pub database: Option<PathBuf>,
    /// Whether to run with no database at all, keeping everything in memory.
    pub ephemeral: bool,
    /// Whether to print authorization and context events as they are published.
    pub verbose: bool,
    /// Where to append a JSONL measurement record of the run, if anywhere.
    ///
    /// See [`crate::recorder`] for exactly what is and is not written.
    pub record: Option<PathBuf>,
    /// The host process's socket, if this run is to be a client of one.
    ///
    /// Present changes what `aik` *is*: it assembles no kernel, opens no database and
    /// registers no tool, because a running host process already holds all three.
    ///
    /// Every option that would describe an assembly or an identity — `--write`,
    /// `--no-tools`, `--memory`, `--db`, `--ephemeral`, `--policy`, `--root`, `--model`,
    /// `--agent`, `--user` — is refused alongside it rather than silently ignored, and so is
    /// `--record`, which records events a client never sees. Accepting any of them would
    /// suggest this run could narrow what the host serves, or choose who it acts as. It can
    /// do neither: only the host's own configuration can.
    pub socket: Option<PathBuf>,
}

impl Options {
    /// Which filesystem tools these options ask for.
    pub fn tools(&self) -> ToolSet {
        match (self.no_tools, self.write) {
            (true, _) => ToolSet::None,
            (false, true) => ToolSet::ReadWrite,
            (false, false) => ToolSet::ReadOnly,
        }
    }

    /// Which memory tools these options ask for.
    ///
    /// `--no-tools` means what it says: no tools at all, memory included. It is the one
    /// switch that has to keep meaning that as tools are added, so it is applied here
    /// rather than left for the wiring to remember.
    pub fn memory(&self) -> MemorySet {
        match self.no_tools {
            true => MemorySet::Off,
            false => self.memory.unwrap_or_default(),
        }
    }

    /// Whether this is a one-shot run rather than an interactive session.
    ///
    /// The distinction is a security boundary, not a convenience: an interactive session
    /// attaches an approval responder and a one-shot run does not, so a policy that defers
    /// to a human refuses when there is no human. See [`crate::approval`].
    pub fn is_one_shot(&self) -> bool {
        self.prompt.is_some()
    }
}

/// What parsing produced: something to run, or something to print and exit.
#[derive(Debug, Clone, PartialEq)]
pub enum Invocation {
    /// Run with these options.
    Run(Box<Options>),
    /// Review or prune the durable audit trail.
    Audit(Box<AuditOptions>),
    /// Print the help text and exit successfully.
    Help,
    /// Print the audit subcommand's help text and exit successfully.
    AuditHelp,
    /// Print the version and exit successfully.
    Version,
}

/// The help text, printed for `--help` and on a usage error.
pub const HELP: &str = concat!(
    "aik — a terminal frontend for the AI kernel\n",
    "\n",
    "USAGE:\n",
    "    aik [OPTIONS]              start an interactive session\n",
    "    aik [OPTIONS] <PROMPT>...  run one prompt and exit\n",
    "    aik audit [OPTIONS]        review the durable audit trail\n",
    "    aik audit prune --older-than <PERIOD>   remove old audit records\n",
    "\n",
    "OPTIONS:\n",
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
    "        --memory <MODE>  which memory tools to register: off, recall (get, query),\n",
    "                         remember (recall plus put), full (also delete)\n",
    "                         [default: remember]\n",
    "        --db <FILE>      the shared database file [default: the path in\n",
    "                         components.store.db.path, else\n",
    "                         $XDG_DATA_HOME/aik/aik.redb]\n",
    "        --ephemeral      open no database: context, memory and scheduled jobs live\n",
    "                         only for this process and are gone when it exits\n",
    "        --session <ID>   resume this durable session instead of starting a new one;\n",
    "                         list the ones you own with /sessions in an interactive run\n",
    "    -v, --verbose        print authorization and context events as they happen\n",
    "    -R, --record <FILE>  append a JSONL measurement record of the run to FILE\n",
    "                         (counts and timings only — see docs/MEASUREMENTS.md)\n",
    "    -s, --socket <PATH>  talk to a running `aikd` on this socket instead of\n",
    "                         assembling a kernel here; see HOST PROCESS below\n",
    "    -h, --help           print this help\n",
    "    -V, --version        print the version\n",
    "\n",
    "STORAGE:\n",
    "    By default the transcript, the agent's memories and any persistent scheduled job\n",
    "    are kept in one database, created 0600 in a 0700 directory. `--ephemeral` keeps\n",
    "    all three in memory instead, so nothing this run says or learns reaches the disk.\n",
    "\n",
    "APPROVALS:\n",
    "    An interactive session answers `require_approval` from the terminal. A one-shot\n",
    "    run does not attach a responder, so a policy that defers to a human refuses\n",
    "    instead of waiting for one who is not there.\n",
    "\n",
    "HOST PROCESS:\n",
    "    Only one process may hold the database: it is locked while open. `aikd` is the\n",
    "    process that holds it, runs the schedule, and serves clients over a local socket\n",
    "    that only your account can reach. With one running, pass --socket (or set\n",
    "    AIK_SOCKET) and this command becomes a client of it; without one, `aik` assembles\n",
    "    its own kernel exactly as before.\n",
    "\n",
    "AUDIT:\n",
    "    Every authorization decision and every tool call is recorded durably. Review it\n",
    "    with `aik audit`; see `aik audit --help`. A prompt whose first word is `audit`\n",
    "    needs `aik -- audit ...` to stay a prompt.\n",
);

/// The help text for the `audit` subcommand.
pub const AUDIT_HELP: &str = concat!(
    "aik audit — review what this system was allowed to do\n",
    "\n",
    "USAGE:\n",
    "    aik audit [OPTIONS]                     show recent audit records\n",
    "    aik audit prune --older-than <PERIOD>   remove records older than PERIOD\n",
    "\n",
    "OPTIONS:\n",
    "        --principal <ID>  only records this principal is a party to: what they did,\n",
    "                          and what was done on their behalf\n",
    "        --tool <NAME>     only records naming this tool\n",
    "        --correlation <ID>  only records from one operation, which is how a decision\n",
    "                          is joined to the call it gated\n",
    "        --since <PERIOD>  only records from the last PERIOD (30m, 12h, 7d, 4w)\n",
    "        --kind <KIND>     authorization, invocation, gap or retention\n",
    "        --refused         only records of something being refused\n",
    "        --limit <N>       at most N records, newest first [default: 50]\n",
    "        --json            one JSON object per line instead of a table\n",
    "        --older-than <PERIOD>  (prune) remove records at or before this far back\n",
    "        --dry-run         (prune) report what would go without removing it\n",
    "    -u, --user <ID>       the identity to read as [default: user]\n",
    "    -c, --config <FILE>   JSON configuration file, consulted for the database path\n",
    "        --db <FILE>       the shared database file\n",
    "    -s, --socket <PATH>   ask a running `aikd` on this socket instead of opening the\n",
    "                          database directly, which it holds locked while it runs\n",
    "    -h, --help            print this help\n",
    "\n",
    "WHAT YOU CAN SEE:\n",
    "    The trail shows what the reading identity did and what was done on its behalf.\n",
    "    Another principal's records are not shown, and naming them in --principal does\n",
    "    not change that. Records saying the trail is incomplete — a dropped-event gap, a\n",
    "    retention sweep — are shown to every reader and are never hidden by --principal,\n",
    "    so no identity and no principal filter can make a short trail look complete.\n",
    "    Asking for one kind or one operation does exclude them, because that is what you\n",
    "    asked for; `aik audit --kind gap` lists them on their own.\n",
    "\n",
    "PRUNING:\n",
    "    Nothing is ever removed unless somebody asks. `prune` needs an explicit period,\n",
    "    never removes a gap or a retention marker, and writes a record of what it removed.\n",
    "    Use --dry-run first.\n",
);

/// Parses command-line arguments, excluding the program name.
///
/// Accepts `--flag value` and `--flag=value`, clustered short flags are *not* supported,
/// and `--` ends option parsing. Unknown options are an error rather than being passed
/// through as prompt text: silently treating `--wirte` as part of the question is how a
/// person ends up believing they enabled something they did not.
pub fn parse<I, S>(args: I) -> Result<Invocation>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut options = Options::default();
    let mut words: Vec<String> = Vec::new();
    let mut rest = args.into_iter().map(Into::into).collect::<Vec<_>>();

    // Checked before anything else, and only in first position: a subcommand that could
    // appear anywhere would change what an existing prompt means depending on where a word
    // happened to fall.
    if rest.first().map(String::as_str) == Some(AUDIT_COMMAND) {
        return parse_audit(rest.into_iter().skip(1));
    }

    rest.reverse();
    let mut literal = false;

    while let Some(argument) = rest.pop() {
        if literal {
            words.push(argument);
            continue;
        }

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
            "--" => literal = true,
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "-m" | "--model" => options.model = Some(value(&flag)?),
            "-a" | "--agent" => options.agent = Some(value(&flag)?),
            "-u" | "--user" => options.user = Some(value(&flag)?),
            "-r" | "--root" => options.root = Some(PathBuf::from(value(&flag)?)),
            "-c" | "--config" => options.config = Some(PathBuf::from(value(&flag)?)),
            "-p" | "--policy" => options.policy = Some(PathBuf::from(value(&flag)?)),
            "--write" => options.write = true,
            "--no-tools" => options.no_tools = true,
            "--memory" => {
                let raw = value(&flag)?;
                options.memory = Some(MemorySet::parse(&raw).map_err(|error| {
                    usage(format!(
                        "`--memory` takes one of off, recall, remember, full; got `{raw}` ({error})"
                    ))
                })?);
            }
            "--db" => options.database = Some(PathBuf::from(value(&flag)?)),
            "--ephemeral" => options.ephemeral = true,
            "--session" => options.session = Some(parse_session(&value(&flag)?)?),
            "-v" | "--verbose" => options.verbose = true,
            "-R" | "--record" => options.record = Some(PathBuf::from(value(&flag)?)),
            "-s" | "--socket" => options.socket = Some(PathBuf::from(value(&flag)?)),
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(usage(format!("unknown option `{other}`")));
            }
            _ => words.push(argument),
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

    if options.ephemeral && options.database.is_some() {
        return Err(usage(
            "`--ephemeral` and `--db` contradict each other".to_owned(),
        ));
    }

    // Resuming needs somewhere for the session to have been kept. `--ephemeral` guarantees
    // there is nowhere, so the combination cannot be satisfied — and silently starting a new
    // session instead would be the one behaviour `--session` exists to rule out.
    if options.ephemeral && options.session.is_some() {
        return Err(usage(
            "`--ephemeral` and `--session` contradict each other: an ephemeral run keeps no \
             session to resume"
                .to_owned(),
        ));
    }

    // Every option below describes how a system is *assembled*, and a client assembles
    // nothing. Refusing them is the honest answer: a `--write` accepted and ignored would
    // read as "this connection may write", and a `--no-tools` accepted and ignored would
    // read as the far more dangerous converse.
    if options.socket.is_some() {
        let conflicting = [
            ("--write", options.write),
            ("--no-tools", options.no_tools),
            ("--memory", options.memory.is_some()),
            ("--db", options.database.is_some()),
            ("--ephemeral", options.ephemeral),
            ("--policy", options.policy.is_some()),
            ("--root", options.root.is_some()),
            ("--model", options.model.is_some()),
            ("--agent", options.agent.is_some()),
            ("--user", options.user.is_some()),
            ("--record", options.record.is_some()),
        ];
        if let Some((name, _)) = conflicting.into_iter().find(|(_, given)| *given) {
            return Err(usage(format!(
                "`--socket` and `{name}` contradict each other: a client assembles nothing, \
                 so what it may reach is the host process's configuration and not this \
                 command's"
            )));
        }
    }

    if !words.is_empty() {
        options.prompt = Some(words.join(" "));
    }

    Ok(Invocation::Run(Box::new(options)))
}

/// Parses a session id, refusing anything that is not one.
///
/// A malformed id is a usage error rather than a session that turns out not to exist: the
/// two would be reported identically otherwise, and only one of them is worth retyping.
fn parse_session(raw: &str) -> Result<SessionId> {
    raw.parse().map_err(|_| {
        usage(format!(
            "`--session` takes the id of an existing session; `{raw}` is not one"
        ))
    })
}

/// The `audit` subcommand: reviewing the durable trail, or pruning it.
///
/// A subcommand rather than a flag on a run, because it is not a run: it starts no model, no
/// agent and no tools, opens the database read-only in every sense that matters, and answers
/// one question. Wiring it as a mode of `aik <prompt>` would mean a conversation that could
/// also, depending on flags, delete records.
///
/// The cost of a subcommand is that `aik audit the logs` is now a usage error rather than a
/// prompt. That is the trade every subcommand makes, it is loud rather than silent, and
/// `aik -- audit the logs` still says what it used to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditOptions {
    /// What to do with the trail.
    pub command: AuditCommand,
    /// Where the shared database lives, overriding configuration and the XDG default.
    pub database: Option<PathBuf>,
    /// A JSON configuration file, consulted for the database path.
    pub config: Option<PathBuf>,
    /// The identity the review runs as, overriding configuration.
    ///
    /// This is *the reader*, and it decides what comes back: the trail shows what this
    /// principal did and what was done on their behalf, and nothing else. See
    /// [`crate::audit`] for why it is the user rather than the agent.
    pub user: Option<String>,
    /// The host process's socket, if the trail is to be read through one.
    ///
    /// The database is locked by whichever process has it open, so a review while `aikd`
    /// runs has to go through `aikd`. It reads under the same identity either way — see
    /// [`crate::audit`] — because the socket already establishes that the caller is the
    /// account that owns the database, which is the same thing opening the file establishes.
    pub socket: Option<PathBuf>,
}

/// Reviewing, or pruning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditCommand {
    /// Show matching records.
    Review(Box<AuditFilters>),
    /// Remove records older than a period, or say how many that would be.
    Prune {
        /// Records at or before this far in the past are removed.
        older_than: Duration,
        /// Count what would go instead of removing it.
        dry_run: bool,
    },
}

/// Which records a review shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilters {
    /// Only records this principal is a party to.
    pub principal: Option<String>,
    /// Only records naming this tool.
    pub tool: Option<String>,
    /// Only records belonging to this one operation.
    pub correlation: Option<CorrelationId>,
    /// Only records from this far back.
    pub since: Option<Duration>,
    /// Only this sort of entry.
    pub kind: Option<AuditEntryKind>,
    /// Only records of something being refused.
    pub refusals: bool,
    /// At most this many records, newest first.
    pub limit: Option<usize>,
    /// Print one JSON object per line instead of a table.
    pub json: bool,
}

/// The default number of records a review shows.
///
/// A screenful. An audit trail is the longest collection this system keeps, so the useful
/// default is "the recent past", with `--limit` for anyone who wants more.
pub const DEFAULT_AUDIT_LIMIT: usize = 50;

/// The name of the audit subcommand, as the first argument.
pub const AUDIT_COMMAND: &str = "audit";

/// The name of the prune subcommand, as the argument after `audit`.
pub const PRUNE_COMMAND: &str = "prune";

/// Parses `aik audit ...`, given the arguments after the word `audit`.
fn parse_audit<I: Iterator<Item = String>>(args: I) -> Result<Invocation> {
    let mut rest: Vec<String> = args.collect();
    rest.reverse();

    let mut prune = false;
    if rest.last().map(String::as_str) == Some(PRUNE_COMMAND) {
        rest.pop();
        prune = true;
    }

    let mut options = AuditOptions {
        command: AuditCommand::Review(Box::default()),
        database: None,
        config: None,
        user: None,
        socket: None,
    };
    let mut filters = AuditFilters::default();
    let mut older_than: Option<Duration> = None;
    let mut dry_run = false;

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
                    .ok_or_else(|| audit_usage(format!("`{name}` needs a value"))),
            }
        };

        match flag.as_str() {
            "-h" | "--help" => return Ok(Invocation::AuditHelp),
            "--db" => options.database = Some(PathBuf::from(value(&flag)?)),
            "-c" | "--config" => options.config = Some(PathBuf::from(value(&flag)?)),
            "-u" | "--user" => options.user = Some(value(&flag)?),
            "-s" | "--socket" => options.socket = Some(PathBuf::from(value(&flag)?)),

            "--principal" => filters.principal = Some(value(&flag)?),
            "--tool" => filters.tool = Some(value(&flag)?),
            "--correlation" => filters.correlation = Some(parse_correlation(&value(&flag)?)?),
            "--since" => filters.since = Some(parse_duration(&value(&flag)?, "--since")?),
            "--kind" => filters.kind = Some(parse_kind(&value(&flag)?)?),
            "--refused" => filters.refusals = true,
            "--limit" => filters.limit = Some(parse_limit(&value(&flag)?)?),
            "--json" => filters.json = true,
            "--older-than" => older_than = Some(parse_duration(&value(&flag)?, "--older-than")?),
            "--dry-run" => dry_run = true,
            other => {
                return Err(audit_usage(format!(
                    "unknown option `{other}` for `{PROGRAM} {AUDIT_COMMAND}`"
                )));
            }
        }
    }

    // Each mode refuses the other's options rather than ignoring them. A person who typed
    // `--older-than` at a review meant to delete something, and silently showing them a list
    // instead would leave them believing they had.
    if prune {
        if filters != AuditFilters::default() {
            return Err(audit_usage(format!(
                "`{PROGRAM} {AUDIT_COMMAND} {PRUNE_COMMAND}` takes only --older-than, \
                 --dry-run, --db, --config, --socket and --user"
            )));
        }
        let older_than = older_than.ok_or_else(|| {
            audit_usage(format!(
                "`{PROGRAM} {AUDIT_COMMAND} {PRUNE_COMMAND}` needs `--older-than <PERIOD>`; \
                 there is no default, because the default for destroying an audit trail is \
                 not to"
            ))
        })?;
        options.command = AuditCommand::Prune {
            older_than,
            dry_run,
        };
    } else {
        if older_than.is_some() || dry_run {
            return Err(audit_usage(format!(
                "`--older-than` and `--dry-run` belong to `{PROGRAM} {AUDIT_COMMAND} \
                 {PRUNE_COMMAND}`"
            )));
        }
        options.command = AuditCommand::Review(Box::new(filters));
    }

    // A socket names a *host*, and the host decides both which database it holds and which
    // identity a review reads as. Accepting either alongside it and ignoring them would let
    // somebody believe they had reviewed a different trail, or reviewed it as somebody else.
    if options.socket.is_some() {
        if options.database.is_some() {
            return Err(audit_usage(
                "`--socket` and `--db` contradict each other: the host holds the database, and \
                 which one that is is its configuration rather than this command's"
                    .to_owned(),
            ));
        }
        if options.user.is_some() {
            return Err(audit_usage(
                "`--socket` and `--user` contradict each other: a review through a host reads \
                 as the identity that host was configured with"
                    .to_owned(),
            ));
        }
    }

    Ok(Invocation::Audit(Box::new(options)))
}

/// Parses a correlation id, refusing anything that is not one.
fn parse_correlation(raw: &str) -> Result<CorrelationId> {
    raw.parse().map_err(|_| {
        audit_usage(format!(
            "`--correlation` takes the id of an operation; `{raw}` is not one"
        ))
    })
}

/// Parses an entry kind, or explains what the accepted ones are.
fn parse_kind(raw: &str) -> Result<AuditEntryKind> {
    match raw {
        "authorization" => Ok(AuditEntryKind::Authorization),
        "invocation" => Ok(AuditEntryKind::Invocation),
        "gap" => Ok(AuditEntryKind::Gap),
        "retention" => Ok(AuditEntryKind::Retention),
        other => Err(audit_usage(format!(
            "`--kind` takes one of authorization, invocation, gap, retention; got `{other}`"
        ))),
    }
}

/// Parses a record limit, refusing zero.
fn parse_limit(raw: &str) -> Result<usize> {
    match raw.parse::<usize>() {
        Ok(0) | Err(_) => Err(audit_usage(format!(
            "`--limit` takes a positive number of records; got `{raw}`"
        ))),
        Ok(limit) => Ok(limit),
    }
}

/// Parses a period such as `30m`, `12h`, `7d` or `4w`.
///
/// A unit is required. A bare number would have to mean seconds, days or milliseconds by
/// convention, and the flag that consumes this can destroy an audit trail — a convention is
/// not a good enough reason to guess which of those somebody meant.
pub fn parse_duration(raw: &str, flag: &str) -> Result<Duration> {
    let (digits, unit) = raw.split_at(
        raw.find(|character: char| !character.is_ascii_digit())
            .ok_or_else(|| {
                audit_usage(format!(
                    "`{flag}` takes a period with a unit, such as 30m, 12h, 7d or 4w; got \
                     `{raw}`"
                ))
            })?,
    );

    let seconds_per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        other => {
            return Err(audit_usage(format!(
                "`{flag}` takes a unit of s, m, h, d or w; got `{other}`"
            )));
        }
    };

    let count: u64 = digits.parse().map_err(|_| {
        audit_usage(format!(
            "`{flag}` takes a period with a unit, such as 30m, 12h, 7d or 4w; got `{raw}`"
        ))
    })?;
    if count == 0 {
        return Err(audit_usage(format!(
            "`{flag}` takes a period longer than zero"
        )));
    }

    count
        .checked_mul(seconds_per_unit)
        .map(Duration::from_secs)
        .ok_or_else(|| {
            audit_usage(format!(
                "`{flag}`: `{raw}` is longer than any period can be"
            ))
        })
}

fn usage(message: String) -> Error {
    Error::InvalidArgument(format!("{message}\n\nRun `{PROGRAM} --help` for usage."))
}

/// As [`usage`], pointing at the subcommand's own help rather than the program's.
///
/// A person who mistyped `aik audit --older-than 30` is not helped by being sent to a page
/// that does not mention `--older-than`.
fn audit_usage(message: String) -> Error {
    Error::InvalidArgument(format!(
        "{message}\n\nRun `{PROGRAM} {AUDIT_COMMAND} --help` for usage."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> Options {
        match parse(args.iter().copied()).expect("valid arguments") {
            Invocation::Run(options) => *options,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_means_an_interactive_session_with_read_only_tools() {
        let parsed = options(&[]);
        assert_eq!(parsed, Options::default());
        assert!(!parsed.is_one_shot());
        assert_eq!(parsed.tools(), ToolSet::ReadOnly);
    }

    #[test]
    fn a_positional_argument_makes_it_one_shot() {
        let parsed = options(&["what", "is", "in", "src?"]);
        assert_eq!(parsed.prompt.as_deref(), Some("what is in src?"));
        assert!(parsed.is_one_shot());
    }

    #[test]
    fn options_accept_both_separated_and_inline_values() {
        let separated = options(&["--model", "llama3.2", "--root", "/tmp"]);
        let inline = options(&["--model=llama3.2", "--root=/tmp"]);
        assert_eq!(separated, inline);
        assert_eq!(separated.model.as_deref(), Some("llama3.2"));
        assert_eq!(
            separated.root.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
    }

    #[test]
    fn short_options_mirror_long_ones() {
        assert_eq!(
            options(&["-m", "m", "-a", "a", "-u", "u", "-v"]),
            options(&["--model", "m", "--agent", "a", "--user", "u", "--verbose"]),
        );
    }

    #[test]
    fn write_and_no_tools_select_the_tool_set() {
        assert_eq!(options(&["--write"]).tools(), ToolSet::ReadWrite);
        assert_eq!(options(&["--no-tools"]).tools(), ToolSet::None);
    }

    #[test]
    fn memory_defaults_to_recalling_and_recording_but_never_forgetting() {
        assert_eq!(options(&[]).memory(), MemorySet::Remember);
        assert_ne!(
            options(&[]).memory(),
            MemorySet::Full,
            "deletion must never be reachable without somebody asking for it",
        );
    }

    #[test]
    fn every_memory_mode_is_selectable_by_name() {
        for (name, expected) in [
            ("off", MemorySet::Off),
            ("recall", MemorySet::Recall),
            ("remember", MemorySet::Remember),
            ("full", MemorySet::Full),
        ] {
            assert_eq!(options(&["--memory", name]).memory(), expected);
            assert_eq!(expected.as_str(), name);
        }
    }

    #[test]
    fn an_unknown_memory_mode_is_rejected_rather_than_taken_as_the_default() {
        // Silently falling back to `remember` on a typo would hand an agent a write tool
        // somebody was trying to take away.
        let error = parse(["--memory", "raed-only"]).unwrap_err();
        assert!(error.to_string().contains("raed-only"), "{error}");
    }

    #[test]
    fn no_tools_means_no_memory_tools_either() {
        assert_eq!(options(&["--no-tools"]).memory(), MemorySet::Off);
    }

    #[test]
    fn asking_for_no_tools_and_a_memory_mode_at_once_is_an_error() {
        let error = parse(["--no-tools", "--memory", "full"]).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }

    #[test]
    fn a_database_path_is_accepted_in_both_forms() {
        assert_eq!(
            options(&["--db", "/srv/aik.redb"]),
            options(&["--db=/srv/aik.redb"])
        );
        assert_eq!(
            options(&["--db", "/srv/aik.redb"]).database.as_deref(),
            Some(std::path::Path::new("/srv/aik.redb")),
        );
    }

    #[test]
    fn a_database_is_opened_by_default_and_suppressed_by_ephemeral() {
        assert!(!options(&[]).ephemeral);
        assert!(options(&["--ephemeral"]).ephemeral);
    }

    #[test]
    fn asking_for_no_database_and_a_database_path_at_once_is_an_error() {
        let error = parse(["--ephemeral", "--db", "/srv/aik.redb"]).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }

    #[test]
    fn asking_for_no_tools_and_a_write_tool_at_once_is_an_error() {
        // Rather than picking one: a person who typed both does not agree with themselves
        // about what the agent may do, and guessing at it is how the wrong guess ships.
        let error = parse(["--no-tools", "--write"]).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }

    #[test]
    fn an_unknown_option_is_rejected_rather_than_taken_as_prompt_text() {
        let error = parse(["--wirte", "hello"]).unwrap_err();
        assert!(error.to_string().contains("--wirte"), "{error}");
    }

    #[test]
    fn a_missing_value_is_rejected() {
        let error = parse(["--model"]).unwrap_err();
        assert!(error.to_string().contains("--model"), "{error}");
    }

    #[test]
    fn everything_after_a_double_dash_is_prompt_text() {
        let parsed = options(&["--", "--not-an-option", "please"]);
        assert_eq!(parsed.prompt.as_deref(), Some("--not-an-option please"));
        assert!(!parsed.verbose);
    }

    #[test]
    fn a_bare_dash_is_prompt_text_rather_than_an_option() {
        assert_eq!(options(&["-"]).prompt.as_deref(), Some("-"));
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(["--help"]).unwrap(), Invocation::Help);
        assert_eq!(parse(["-h", "--nonsense"]).unwrap(), Invocation::Help);
        assert_eq!(parse(["--version"]).unwrap(), Invocation::Version);
    }

    #[test]
    fn the_help_text_documents_every_option_it_parses() {
        for flag in [
            "--model",
            "--agent",
            "--user",
            "--root",
            "--config",
            "--policy",
            "--write",
            "--no-tools",
            "--memory",
            "--db",
            "--ephemeral",
            "--session",
            "--verbose",
            "--record",
            "--help",
            "--version",
        ] {
            assert!(HELP.contains(flag), "`{flag}` is undocumented");
        }
    }

    #[test]
    fn a_session_to_resume_is_accepted_in_both_forms() {
        let id = SessionId::new();
        let separated = options(&["--session", &id.to_string()]);
        let inline = options(&[&format!("--session={id}")]);
        assert_eq!(separated, inline);
        assert_eq!(separated.session, Some(id));
    }

    #[test]
    fn no_session_is_resumed_by_default() {
        assert!(options(&[]).session.is_none());
    }

    #[test]
    fn a_malformed_session_id_is_a_usage_error_not_a_missing_session() {
        let error = parse(["--session", "not-a-uuid"]).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
        assert!(error.to_string().contains("not-a-uuid"), "{error}");
    }

    #[test]
    fn asking_to_resume_a_session_without_a_database_is_an_error() {
        // Rather than starting a fresh one: a person who asked to resume a specific
        // conversation has not agreed to talk to an empty one instead.
        let error = parse(["--ephemeral", "--session", &SessionId::new().to_string()]).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }

    #[test]
    fn a_record_path_is_accepted_in_both_forms() {
        let separated = options(&["--record", "run.jsonl"]);
        let inline = options(&["--record=run.jsonl"]);
        let short = options(&["-R", "run.jsonl"]);
        assert_eq!(separated, inline);
        assert_eq!(separated, short);
        assert_eq!(
            separated.record.as_deref(),
            Some(std::path::Path::new("run.jsonl"))
        );
    }

    #[test]
    fn no_record_path_is_configured_by_default() {
        assert!(options(&[]).record.is_none());
    }

    fn audit(args: &[&str]) -> AuditOptions {
        match parse(args.iter().copied()).expect("valid arguments") {
            Invocation::Audit(options) => *options,
            other => panic!("expected an audit command, got {other:?}"),
        }
    }

    fn filters(args: &[&str]) -> AuditFilters {
        match audit(args).command {
            AuditCommand::Review(filters) => *filters,
            other => panic!("expected a review, got {other:?}"),
        }
    }

    #[test]
    fn audit_is_a_subcommand_only_in_first_position() {
        assert!(matches!(parse(["audit"]).unwrap(), Invocation::Audit(_),));

        // Anywhere else it is what it always was: a word in a prompt.
        let parsed = options(&["what", "does", "audit", "mean"]);
        assert_eq!(parsed.prompt.as_deref(), Some("what does audit mean"));
    }

    #[test]
    fn a_prompt_beginning_with_audit_is_still_reachable() {
        let parsed = options(&["--", "audit", "the", "logs"]);
        assert_eq!(parsed.prompt.as_deref(), Some("audit the logs"));
    }

    #[test]
    fn a_bare_audit_reviews_with_no_filters() {
        assert_eq!(filters(&["audit"]), AuditFilters::default());
        assert_eq!(audit(&["audit"]).database, None);
    }

    #[test]
    fn every_review_filter_is_parsed_in_both_forms() {
        let correlation = CorrelationId::new();
        let separated = filters(&[
            "audit",
            "--principal",
            "alice",
            "--tool",
            "filesystem.read",
            "--correlation",
            &correlation.to_string(),
            "--since",
            "12h",
            "--kind",
            "invocation",
            "--limit",
            "5",
            "--refused",
            "--json",
        ]);
        let inline = filters(&[
            "audit",
            "--principal=alice",
            "--tool=filesystem.read",
            &format!("--correlation={correlation}"),
            "--since=12h",
            "--kind=invocation",
            "--limit=5",
            "--refused",
            "--json",
        ]);
        assert_eq!(separated, inline);
        assert_eq!(separated.principal.as_deref(), Some("alice"));
        assert_eq!(separated.tool.as_deref(), Some("filesystem.read"));
        assert_eq!(separated.correlation, Some(correlation));
        assert_eq!(separated.since, Some(Duration::from_secs(12 * 60 * 60)));
        assert_eq!(separated.kind, Some(AuditEntryKind::Invocation));
        assert_eq!(separated.limit, Some(5));
        assert!(separated.refusals);
        assert!(separated.json);
    }

    #[test]
    fn every_entry_kind_is_selectable_by_name() {
        for (name, expected) in [
            ("authorization", AuditEntryKind::Authorization),
            ("invocation", AuditEntryKind::Invocation),
            ("gap", AuditEntryKind::Gap),
            ("retention", AuditEntryKind::Retention),
        ] {
            assert_eq!(filters(&["audit", "--kind", name]).kind, Some(expected));
        }
        assert!(parse(["audit", "--kind", "nonsense"]).is_err());
    }

    #[test]
    fn a_review_shares_the_database_and_identity_options_a_run_uses() {
        let parsed = audit(&[
            "audit",
            "--db",
            "/srv/aik.redb",
            "--config",
            "/etc/aik.json",
            "--user",
            "alice",
        ]);
        assert_eq!(
            parsed.database.as_deref(),
            Some(std::path::Path::new("/srv/aik.redb"))
        );
        assert_eq!(
            parsed.config.as_deref(),
            Some(std::path::Path::new("/etc/aik.json"))
        );
        assert_eq!(parsed.user.as_deref(), Some("alice"));
    }

    #[test]
    fn pruning_needs_an_explicit_period() {
        // There is no default, deliberately: the default for destroying an audit trail is
        // not to.
        let error = parse(["audit", "prune"]).unwrap_err();
        assert!(error.to_string().contains("--older-than"), "{error}");
    }

    #[test]
    fn pruning_parses_its_period_and_its_dry_run() {
        assert_eq!(
            audit(&["audit", "prune", "--older-than", "90d"]).command,
            AuditCommand::Prune {
                older_than: Duration::from_secs(90 * 24 * 60 * 60),
                dry_run: false,
            }
        );
        assert_eq!(
            audit(&["audit", "prune", "--older-than=4w", "--dry-run"]).command,
            AuditCommand::Prune {
                older_than: Duration::from_secs(4 * 7 * 24 * 60 * 60),
                dry_run: true,
            }
        );
    }

    #[test]
    fn each_audit_mode_refuses_the_others_options() {
        // Rather than ignoring them: somebody who typed `--older-than` at a review meant to
        // delete something, and showing them a list instead would leave them believing they
        // had.
        let at_review = parse(["audit", "--older-than", "7d"]).unwrap_err();
        assert!(at_review.to_string().contains("prune"), "{at_review}");

        let at_prune = parse(["audit", "prune", "--older-than", "7d", "--limit", "5"]).unwrap_err();
        assert!(matches!(at_prune, Error::InvalidArgument(_)), "{at_prune}");
    }

    #[test]
    fn an_unknown_audit_option_is_rejected() {
        let error = parse(["audit", "--principle", "alice"]).unwrap_err();
        assert!(error.to_string().contains("--principle"), "{error}");
    }

    #[test]
    fn a_mistake_in_the_subcommand_points_at_the_subcommand_s_help() {
        // Being sent to a page that does not mention the flag you mistyped is worse than
        // being sent nowhere.
        for arguments in [
            vec!["audit", "--older-than", "30"],
            vec!["audit", "--limit", "0"],
            vec!["audit", "prune"],
        ] {
            let error = parse(arguments.clone()).unwrap_err().to_string();
            assert!(
                error.contains("aik audit --help"),
                "{arguments:?} was sent to the wrong help: {error}"
            );
        }

        // And a mistake in a run still points at the program's own help.
        let error = parse(["--wirte"]).unwrap_err().to_string();
        assert!(error.contains("aik --help"), "{error}");
        assert!(!error.contains("aik audit --help"), "{error}");
    }

    #[test]
    fn audit_help_is_its_own_help() {
        assert_eq!(parse(["audit", "--help"]).unwrap(), Invocation::AuditHelp);
        assert_eq!(parse(["audit", "-h"]).unwrap(), Invocation::AuditHelp);
    }

    #[test]
    fn a_period_is_parsed_in_every_unit_it_documents() {
        for (raw, expected) in [
            ("90s", Duration::from_secs(90)),
            ("30m", Duration::from_secs(30 * 60)),
            ("12h", Duration::from_secs(12 * 60 * 60)),
            ("7d", Duration::from_secs(7 * 24 * 60 * 60)),
            ("4w", Duration::from_secs(4 * 7 * 24 * 60 * 60)),
        ] {
            assert_eq!(parse_duration(raw, "--since").unwrap(), expected, "{raw}");
        }
    }

    #[test]
    fn a_period_without_a_unit_is_refused_rather_than_guessed() {
        // The flag that consumes this can destroy an audit trail. "30" could be seconds,
        // days or milliseconds by convention, and a convention is not a good enough reason
        // to pick one.
        for raw in ["30", "", "d", "-7d", "7y", "7 d"] {
            assert!(parse_duration(raw, "--older-than").is_err(), "{raw}");
        }
    }

    #[test]
    fn a_period_of_zero_is_refused() {
        assert!(parse_duration("0d", "--older-than").is_err());
    }

    #[test]
    fn an_absurd_period_is_refused_rather_than_wrapping() {
        let error = parse_duration("99999999999999999999w", "--older-than").unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }

    #[test]
    fn a_limit_of_zero_is_refused() {
        // A review that returned nothing because the limit was zero would look exactly like
        // a trail with nothing in it.
        assert!(parse(["audit", "--limit", "0"]).is_err());
        assert!(parse(["audit", "--limit", "many"]).is_err());
    }

    #[test]
    fn the_audit_help_text_documents_every_option_it_parses() {
        for flag in [
            "--principal",
            "--tool",
            "--correlation",
            "--since",
            "--kind",
            "--refused",
            "--limit",
            "--json",
            "--older-than",
            "--dry-run",
            "--user",
            "--config",
            "--db",
            "--help",
        ] {
            assert!(AUDIT_HELP.contains(flag), "`{flag}` is undocumented");
        }
    }

    #[test]
    fn the_main_help_text_points_at_the_audit_subcommand() {
        assert!(HELP.contains("aik audit"));
    }
}
