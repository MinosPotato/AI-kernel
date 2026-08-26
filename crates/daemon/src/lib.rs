//! The AI kernel's host process.
//!
//! One process owns the durable database and the kernel over it, and everything else talks to
//! that process. Two facts make that the only arrangement that works:
//!
//! * **redb locks the database file.** Exactly one process may have it open, which is what
//!   lets a write spanning the transcript, the memory store, the schedule and the audit trail
//!   be one transaction. A second `aik` started while the first is running does not get a
//!   second view of the data; it does not start.
//! * **A schedule needs something that is always there.** A job that fires at 3am fires in
//!   whatever process is running at 3am. A terminal is not that process, and a terminal that
//!   *were* would interleave unattended agent turns with somebody's conversation.
//!
//! ```text
//!            aik --socket …            aik audit --socket …
//!                  │                            │
//!                  └────────── Unix socket ─────┘
//!                                   │  0600 in a 0700 directory
//!                                   │  peer uid, then token, then version
//!                              ┌────▼────┐
//!                              │  aikd   │
//!                              │ ┌─────┐ │  Server   — accepts, bounds, stops
//!                              │ │Host │ │  Host     — turns a request into a call
//!                              │ └──┬──┘ │
//!                              └────│────┘
//!                        aik_runtime::wiring::assemble
//!                                   │
//!            ┌──────────┬───────────┼───────────┬──────────┐
//!         Agent      Tools       Context     Scheduler   Audit
//!            │       Policy      Memory        Jobs        │
//!            └──────────┴──────── one redb database ───────┘
//! ```
//!
//! # What this crate adds, and what it must not
//!
//! It adds a lifetime and a door. It assembles nothing itself —
//! [`aik_runtime::wiring`] does that, from the same [`RuntimeSettings`](aik_runtime::RuntimeSettings)
//! a terminal run resolves — and it decides nothing about whether an operation may happen.
//!
//! Concretely, the host is bound by the same rules the terminal frontend is:
//!
//! * **It never authorizes.** There is no policy evaluation here, no `Decision`, and no way
//!   to reach a tool. A conversation goes through `dyn Agent`, which holds a
//!   [`ToolRegistry`](aik_api::tool::ToolRegistry) the host cannot reach around.
//! * **It never impersonates.** Every [`ExecutionContext`](aik_api::execution::ExecutionContext)
//!   it builds carries a principal derived from its own settings. A client cannot name one:
//!   there is nowhere on the wire to put it. See [`host`].
//! * **It only ever narrows.** Which tools exist, which memory modes, whether there is a
//!   database — all of it is the same wiring decision a terminal run makes, and none of it can
//!   be widened by a connected client.
//! * **It fails closed.** A peer from another account is refused before its bytes are parsed;
//!   an approval with nobody attached to answer is refused rather than granted; a host that
//!   cannot bind a private socket does not serve on a public one.
//!
//! # What it deliberately does not do
//!
//! * **No network listener.** Unix sockets, peer credentials and file modes are the whole of
//!   the authentication story, and none of the three exists on a network address. Remote
//!   access needs a transport identity that is not a uid and a trust decision that is not a
//!   file mode; adding it means adding both.
//! * **No backgrounding.** No fork, no pid file, no log configuration. A service manager
//!   does that better, and a process that hides itself hides its failures too.
//! * **No second policy.** The host reads the same configured document a terminal run does,
//!   through the same rule-based policy engine. There is
//!   no "daemon policy" that could disagree with it.

pub mod args;
pub mod connection;
pub mod host;
pub mod server;
pub mod settings;

use aik_core::{Error, Result};
use aik_ipc::Listener;
use aik_runtime::wiring;
use tokio_util::sync::CancellationToken;

use crate::args::{HELP, Invocation, Options};
use crate::host::{Host, VERSION};
use crate::server::Server;
use crate::settings::DaemonSettings;

/// Parses arguments and serves, returning the process exit code.
pub async fn main(args: impl IntoIterator<Item = String>) -> i32 {
    match args::parse(args) {
        Ok(Invocation::Help) => {
            print!("{HELP}");
            0
        }
        Ok(Invocation::Version) => {
            println!("{} {VERSION}", args::PROGRAM);
            0
        }
        Ok(Invocation::Serve(options)) => match run(&options).await {
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

/// Serves until a signal asks the host to stop.
pub async fn run(options: &Options) -> Result<()> {
    let settings = DaemonSettings::resolve(options)?;
    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone());
    serve(settings, shutdown).await
}

/// Assembles the kernel and serves it until `shutdown` is cancelled.
pub async fn serve(settings: DaemonSettings, shutdown: CancellationToken) -> Result<()> {
    // Asked before anything is opened, so the common mistake — starting a second host —
    // is reported as what it is rather than as a database that will not open.
    if aik_ipc::is_listening(settings.endpoint.socket()) {
        return Err(Error::AlreadyExists {
            kind: "host process",
            id: format!(
                "{} is already being served; stop that host before starting another",
                settings.endpoint.socket().display()
            ),
        });
    }

    let model = match settings.model() {
        Some(model) => model.clone(),
        None => wiring::first_available_model(&settings.runtime).await?,
    };

    let assembled = wiring::assemble(&settings.runtime, model.clone())?;
    serve_assembled(&settings, model, assembled, shutdown).await
}

/// Starts an already-assembled kernel, serves it, and stops it.
///
/// Split out from [`serve`] so that a test can supply its own kernel — the same wiring, around
/// a scripted model, since the one thing a test cannot have is a language model that reliably
/// says what the test is about. Everything below this line is the shipped path.
///
/// The order is the whole of the lifecycle, and each step is where it is on purpose:
///
/// 1. **The kernel starts first.** The socket's existence is what tells a client there is
///    something to talk to, so it must not exist before there is.
/// 2. **The socket is bound second**, which is also what claims the right to be *the* host: a
///    second one finds the path in use and refuses.
/// 3. **The kernel is shut down after the last client has gone**, which closes the approval
///    broker — refusing whatever was still parked on an answer — and stops the scheduler.
/// 4. **The kernel is dropped last.** redb's exclusive lock belongs to the `Arc<Db>` the
///    kernel's registry owns, so the database file is released when the kernel is dropped and
///    not when it stopped. A restart that only awaited the shutdown would find its own
///    database locked, and would be right to.
pub async fn serve_assembled(
    settings: &DaemonSettings,
    model: aik_api::model::ModelId,
    assembled: wiring::Assembled,
    shutdown: CancellationToken,
) -> Result<()> {
    assembled.kernel.start().await?;

    let outcome = async {
        let host = Host::new(
            &assembled.kernel.context(),
            settings.runtime.clone(),
            model,
            assembled.broker.clone(),
        )?;

        let listener = Listener::bind(settings.endpoint.clone())?;
        banner(settings, &listener);

        Server::new(host, listener, settings.max_connections, shutdown)
            .run()
            .await
    }
    .await;

    let stopped = assembled.kernel.shutdown().await;
    // Explicit, and load-bearing: see step 4 above.
    drop(assembled);
    outcome.and(stopped)
}

/// Cancels `shutdown` on the first `SIGINT` or `SIGTERM`.
///
/// A second signal is not special-cased into an immediate exit. Shutdown is already bounded —
/// connections get [`SHUTDOWN_TIMEOUT`](crate::server::SHUTDOWN_TIMEOUT) and the kernel's own
/// timeout applies after that — and an operator who wants it to stop *now* has `SIGKILL`,
/// which is honest about what it does to a database transaction in flight.
fn spawn_signal_handler(shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    tracing::warn!(%error, "cannot listen for SIGTERM");
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        println!("\nstopping…");
        shutdown.cancel();
    });
}

/// Says what is being served, and where.
///
/// The same things the terminal frontend says at startup, and for the same reason: where a
/// durable record of every conversation lands, and whether anything will be allowed to happen
/// at all, are things an operator should be told rather than discover.
fn banner(settings: &DaemonSettings, listener: &Listener) {
    println!("{} {VERSION}", args::PROGRAM);
    println!(
        "  agent:  {} acting for {}",
        settings.runtime.agent, settings.runtime.user
    );
    println!("  root:   {}", settings.runtime.root.display());
    match settings.runtime.database() {
        Some(path) => println!("  store:  {}", path.display()),
        None => println!("  store:  none (--ephemeral: nothing is written to disk)"),
    }
    println!("  memory: {}", settings.runtime.memory.as_str());
    println!("  summary: {}", settings.runtime.summary_notice());
    println!("  socket: {}", listener.endpoint().socket().display());
    println!("  token:  {}", listener.endpoint().token().display());
    println!("  jobs:   scheduled work runs in this process");
    if !settings.runtime.has_system_prompt() {
        println!(
            "  prompt: none configured, so the agent is told nothing about what it can do.\n\
             \x20         set `{}` to say so.",
            aik_runtime::SYSTEM_PROMPT_KEY,
        );
    }
    if !settings.runtime.has_policy() {
        println!(
            "  policy: none configured, so every tool call will be denied.\n\
             \x20         pass --policy <FILE> to allow anything."
        );
    }
}

/// Renders an error together with its full chain of causes.
fn report(error: &Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    message
}
