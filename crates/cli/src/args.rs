//! Command-line parsing.
//!
//! Hand-written rather than derived from a dependency: there are eleven options, they are
//! all strings, paths or flags, and the whole grammar fits on a screen. What a parser
//! generator would add here is a dependency, not a capability.

use std::path::PathBuf;

use aik_core::{Error, Result};

/// The program name used in help and error messages.
pub const PROGRAM: &str = "aik";

/// Which filesystem tools a run registers.
///
/// A tool that is not registered cannot be reached at all, whatever policy says, so this is
/// the outer of the two limits on what the agent can touch — see
/// [`Options::write`](Options::write).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolSet {
    /// No tools whatsoever. The agent can only talk.
    None,
    /// Reading and listing, confined to the root.
    #[default]
    ReadOnly,
    /// Reading, listing and writing, confined to the root.
    ReadWrite,
}

/// What the user asked for on the command line.
///
/// [`Default`] is the no-arguments invocation: an interactive session, read-only tools, and
/// everything else left to configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    /// The one-shot prompt, or `None` for an interactive session.
    pub prompt: Option<String>,
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
    /// Whether to print authorization and context events as they are published.
    pub verbose: bool,
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
    /// Print the help text and exit successfully.
    Help,
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
    "        --no-tools       register no tools at all\n",
    "    -v, --verbose        print authorization and context events as they happen\n",
    "    -h, --help           print this help\n",
    "    -V, --version        print the version\n",
    "\n",
    "APPROVALS:\n",
    "    An interactive session answers `require_approval` from the terminal. A one-shot\n",
    "    run does not attach a responder, so a policy that defers to a human refuses\n",
    "    instead of waiting for one who is not there.\n",
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
            "-v" | "--verbose" => options.verbose = true,
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

    if !words.is_empty() {
        options.prompt = Some(words.join(" "));
    }

    Ok(Invocation::Run(Box::new(options)))
}

fn usage(message: String) -> Error {
    Error::InvalidArgument(format!("{message}\n\nRun `{PROGRAM} --help` for usage."))
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
            "--verbose",
            "--help",
            "--version",
        ] {
            assert!(HELP.contains(flag), "`{flag}` is undocumented");
        }
    }
}
