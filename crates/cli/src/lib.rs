//! A terminal frontend for the AI kernel.
//!
//! Everything below this crate was a capability: a provider that answers, a registry that
//! authorizes, tools that act, a broker that asks, a store that remembers, a loop that
//! ties them together. None of it could be *used* — there was no way to type a question and
//! no way to answer an approval prompt. This crate is that, and deliberately only that.
//!
//! ```text
//!   argv ──▶ Options ──▶ Settings ──▶ wiring::assemble ──▶ Kernel      (no --socket)
//!                                 └─▶ client::run ──▶ aikd            (--socket)
//!                          │                                 │
//!                          │                                 ▼
//!                          │                          dyn Agent, resolved
//!                          │                                 │
//!                          ▼                                 ▼
//!                    Principal(agent)                  Agent::stream
//!                    on_behalf_of(user)                      │
//!                          └───── ExecutionContext ──────────┤
//!                                                            ▼
//!                                              AgentUpdate ──▶ terminal
//!                                              PendingApproval ──▶ y/N
//! ```
//!
//! # What it is not allowed to be
//!
//! The frontend is the least trustworthy part of the system to put a decision in: it is the
//! part a person is looking at, the part rendering untrusted text, and the part that would
//! be tempting to make "helpful". So it holds none.
//!
//! * **It never authorizes.** There is no policy evaluation here, no `Decision`, no
//!   allow-list of tools it consults before calling one. It cannot invoke a tool at all —
//!   it holds a `dyn Agent`, and the agent holds a
//!   [`ToolRegistry`](aik_api::tool::ToolRegistry) it cannot reach around.
//! * **It never impersonates the user.** Every turn runs as
//!   [`Principal::new(agent, Agent).on_behalf_of(user)`](crate::settings::Settings::principal),
//!   so a policy can distinguish what a person may do from what a model acting for them may
//!   do. Nothing in the frontend can widen that: the principal is built once from resolved
//!   settings and handed to the agent, and a model has no way to influence it.
//! * **It never invents a policy.** With none configured, every tool call is denied, and the
//!   frontend says so at startup rather than shipping a permissive default that a hurried
//!   person would never notice.
//! * **It only ever narrows.** Choosing not to register the write tool, or any tool, removes
//!   a capability. There is no switch here that adds one.
//!
//! # Interactive and one-shot are different security postures
//!
//! An interactive session holds an [`ApprovalGate`](aik_approval::ApprovalGate) — an
//! assertion that a human will really be asked. A one-shot run does not, and the broker
//! refuses every question immediately rather than waiting out a timeout in front of nobody.
//! That is the whole difference, and it is one line in [`run`].
//!
//! # Two ways to reach a kernel
//!
//! With `--socket`, this command assembles nothing: it becomes a client of a running
//! [`aikd`](../aik_daemon/index.html), which already holds the database — exclusively, since
//! redb locks the file — along with the tools and the policy. See [`client`].
//!
//! Every rule above still holds, and holds for a stronger reason: a client cannot authorize,
//! cannot impersonate and cannot widen anything, because it has no registry, no policy engine
//! and no store, and because the protocol has nowhere to name a principal. What decides the
//! two modes is whether a socket was configured, and nothing else.

pub mod approval;
pub mod args;
pub mod audit;
pub mod client;
pub mod console;
pub mod recorder;
pub mod render;
pub mod session;
pub mod settings;
pub mod wiring;

use aik_core::{Error, Result};

use crate::args::{AUDIT_HELP, HELP, Invocation, Options};
use crate::settings::Settings;

/// The version reported by `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parses arguments and runs, returning the process exit code.
///
/// Errors are reported here rather than propagated out of `main`, so that a configuration
/// mistake prints one readable line instead of a debug-formatted error.
pub async fn main(args: impl IntoIterator<Item = String>) -> i32 {
    match args::parse(args) {
        Ok(Invocation::Help) => {
            print!("{HELP}");
            0
        }
        Ok(Invocation::AuditHelp) => {
            print!("{AUDIT_HELP}");
            0
        }
        Ok(Invocation::Version) => {
            println!("{} {VERSION}", args::PROGRAM);
            0
        }
        Ok(Invocation::Audit(options)) => match audit::run(&options).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{}: {}", args::PROGRAM, report(&error));
                1
            }
        },
        Ok(Invocation::Run(options)) => match run(&options).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{}: {}", args::PROGRAM, report(&error));
                1
            }
        },
        Err(error) => {
            eprintln!("{}: {}", args::PROGRAM, report(&error));
            2
        }
    }
}

/// Renders an error together with its full chain of causes.
///
/// `Error`'s own `Display` prints only the context of whichever wraps it, deliberately —
/// see `aik_core::Error::wrap` — so a lower-level failure such as a refused connection or a
/// permission error is still reachable through `std::error::Error::source`, just not part
/// of `{error}` itself. A person reading this on a terminal wants the whole chain, since
/// this is the only place it is ever shown to them.
fn report(error: &Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    message
}

/// Drives one conversation, against a host process or against a kernel of this run's own.
///
/// The fork is one line, and it is the whole difference between the two modes. A client
/// assembles nothing — see [`crate::client`] — because a running host already holds the
/// database, the tools and the policy, and redb would refuse a second process the database
/// anyway.
pub async fn run(options: &Options) -> Result<()> {
    let settings = Settings::resolve(options)?;

    if settings.socket.is_some() {
        return client::run(&settings).await;
    }

    let model = match settings.model() {
        Some(model) => model.clone(),
        None => {
            let model = wiring::first_available_model(&settings).await?;
            println!("model: {model} (first the provider reported)");
            model
        }
    };

    let assembled = wiring::assemble(&settings, model)?;
    assembled.kernel.start().await?;

    let outcome = converse(&assembled, &settings).await;

    // Shut down whatever happened: stopping the approval component closes the broker, so
    // anything still parked on an answer is refused rather than left waiting.
    let shutdown = assembled.kernel.shutdown().await;
    outcome.and(shutdown)
}

async fn converse(assembled: &wiring::Assembled, settings: &Settings) -> Result<()> {
    let kernel = assembled.kernel.context();
    banner(settings);

    // Opened before either session variant, and its failure reported the same way any
    // other startup problem is: named, before a single turn runs, rather than discovered
    // partway through a conversation when the first write silently had nowhere to go.
    let recorder = match &settings.record {
        Some(path) => {
            let recorder = recorder::Recorder::create(path)?;
            println!(
                "  record: appending measurement events to {}",
                path.display()
            );
            Some(recorder)
        }
        None => None,
    };

    match &settings.prompt {
        // One shot: no gate is attached, so the broker has nobody to ask and refuses
        // immediately rather than parking the question in front of an empty terminal.
        Some(prompt) => {
            let mut session = session::stdio(&kernel, settings, None)?;
            if let Some(recorder) = recorder {
                session = session.with_recorder(recorder);
            }
            // Before the turn, so a session that does not exist or is not this run's to use
            // fails with one line instead of after a model call. The store answers; the
            // frontend only reports.
            session.resume(settings).await?;
            session.one_shot(prompt.clone()).await
        }
        // Interactive: subscribing holds a gate for as long as the session lasts, which is
        // what tells the broker a human can actually be asked.
        None => {
            let approvals = assembled.broker.gate().subscribe();
            let mut session = session::stdio(&kernel, settings, Some(approvals))?;
            if let Some(recorder) = recorder {
                session = session.with_recorder(recorder);
            }
            session.resume(settings).await?;
            session.interactive().await.map(|_| ())
        }
    }
}

fn banner(settings: &Settings) {
    println!("{} {VERSION}", args::PROGRAM);
    println!(
        "  agent:  {} acting for {}",
        settings.runtime.agent, settings.runtime.user
    );
    println!("  root:   {}", settings.runtime.root.display());
    // Said out loud for the same reason the absent policy is: where a durable record of
    // every conversation lands is something a person should be told, not something they
    // discover later.
    match settings.database() {
        Some(path) => println!("  store:  {}", path.display()),
        None => println!("  store:  none (--ephemeral: nothing is written to disk)"),
    }
    println!("  memory: {}", settings.runtime.memory.as_str());
    if !settings.runtime.has_system_prompt() {
        println!(
            "  prompt: none configured, so the agent is told nothing about what it can do.\n\
             \x20         set `{}` to say so.",
            aik_runtime::SYSTEM_PROMPT_KEY,
        );
    }
    if !settings.has_policy() {
        println!(
            "  policy: none configured, so every tool call will be denied.\n\
             \x20         pass --policy <FILE> to allow anything."
        );
    }
    if settings.is_one_shot() {
        println!("  approvals: refused (no responder attached in one-shot mode)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_includes_only_the_context_when_there_is_no_source() {
        assert_eq!(report(&Error::other("no such model")), "no such model");
    }

    #[test]
    fn report_walks_the_full_chain_of_causes() {
        let root = std::io::Error::other("connection refused");
        let middle = Error::wrap("sending a completion request to Ollama", root);
        let outer = Error::wrap("talking to the model provider", middle);

        assert_eq!(
            report(&outer),
            "talking to the model provider: sending a completion request to Ollama: connection refused",
        );
    }
}
