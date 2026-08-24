//! `aik --socket`: the same terminal, against a host process instead of its own kernel.
//!
//! # What changes, and what does not
//!
//! What changes is where the kernel is. This mode assembles nothing: no database is opened,
//! no tool is registered, no policy is read, no model is contacted. `aikd` already holds all
//! of that, and holds it exclusively — redb locks the database file, so a terminal that
//! assembled its own kernel while a host was running would simply fail to open it.
//!
//! What does not change is anything about authorization. Every rule this crate's own
//! documentation states still holds, and holds for a stronger reason than before:
//!
//! * **It never authorizes.** It could not: it holds no registry, no policy engine and no
//!   store. It sends a request and prints what comes back.
//! * **It never impersonates the user.** It cannot even try. There is no principal on the
//!   wire — see [`aik_ipc::protocol`] — so the identity every turn runs as is the host's, and
//!   this end has no way to influence it. The host reports it at connection time, and that is
//!   what the banner prints.
//! * **Interactive and one-shot are still different postures.** A conversation connects as
//!   interactive, which makes the host hold an approval gate for as long as the connection
//!   lasts; a one-shot run connects as non-interactive, and the host refuses every question
//!   immediately rather than parking it in front of nobody. The same one line decides it.
//!
//! # Approvals arrive unsolicited
//!
//! A question is not the answer to anything this end asked, and may not even belong to this
//! end's turn: the broker broadcasts to every attached console, and a job firing at 3am asks
//! whoever is there. So questions arrive as their own frames, are put to the person at the
//! terminal, and are answered by quoting the question's id back. A question already answered
//! by another console is reported as no longer waiting, which is exactly what the terminal
//! frontend says when a question expired.

use std::path::Path;

use aik_api::agent::SessionId;
use aik_api::permission::PrincipalId;
use aik_core::{Error, Result};
use aik_ipc::protocol::{Reply, Request, Response, unexpected};
use aik_ipc::{Client, Connected, Endpoint};
use tokio::io::AsyncBufRead;

use crate::approval;
use crate::console::Console;
use crate::render::{self, TurnStats};
use crate::settings::Settings;
use crate::{VERSION, session};

/// How this client names itself to the host. Display only; it is not an identity.
fn client_name() -> String {
    format!("aik {VERSION}")
}

/// Connects to the host named by `settings` and drives one conversation through it.
pub async fn run(settings: &Settings) -> Result<()> {
    let socket = settings
        .socket
        .as_deref()
        .ok_or_else(|| Error::other("no host socket was given"))?;

    let endpoint = Endpoint::at(socket);
    let interactive = !settings.is_one_shot();
    let (client, connected) = Client::connect(&endpoint, &client_name(), interactive).await?;

    banner(socket, &connected);

    let mut session = ClientSession {
        client,
        console: Console::stdio(),
        session: settings.session,
        interactive: connected.interactive,
    };

    match &settings.prompt {
        Some(prompt) => session.turn(prompt.clone()).await,
        None => session.interactive().await,
    }
}

fn banner(socket: &Path, connected: &Connected) {
    println!("{} {VERSION}", crate::args::PROGRAM);
    println!("  host:   {} on {}", connected.host, socket.display());
    match &connected.principal.on_behalf_of {
        Some(user) => println!("  agent:  {} acting for {user}", connected.principal.id),
        None => println!("  agent:  {}", connected.principal.id),
    }
    if connected.interactive {
        println!("  approvals: answered here");
    } else {
        println!("  approvals: refused (no responder attached in one-shot mode)");
    }
    println!("  (this terminal opens no database and registers no tool; the host holds both)");
}

/// One conversation, over one connection.
struct ClientSession<R> {
    client: Client,
    console: Console<R>,
    session: Option<SessionId>,
    interactive: bool,
}

impl<R: AsyncBufRead + Unpin + Send> ClientSession<R> {
    /// Reads and answers prompts until the person stops or input runs out.
    async fn interactive(&mut self) -> Result<()> {
        loop {
            let line = tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c() => {
                    println!();
                    return Ok(());
                }
                line = self.console.ask(session::PROMPT) => line?,
            };

            let Some(line) = line else {
                println!();
                return Ok(());
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(command) = line.strip_prefix('/') {
                if self.command(command).await {
                    return Ok(());
                }
                continue;
            }

            // A failed turn ends the turn, not the session: a refused tool, a model that ran
            // out of time and a session that is not this principal's are all things to report
            // and carry on from.
            if let Err(error) = self.turn(line.to_owned()).await {
                println!("  error: {error}");
            }
        }
    }

    /// Handles a `/command`, reporting whether the session should end.
    async fn command(&mut self, command: &str) -> bool {
        let (verb, argument) = match command.trim().split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (command.trim(), ""),
        };

        let result = match verb {
            "quit" | "q" | "exit" => return true,
            "new" => {
                self.session = None;
                println!("  started a new conversation");
                return false;
            }
            "session" => {
                match self.session {
                    Some(session) => println!("  session {session}"),
                    None => println!("  no session yet; the next prompt starts one"),
                }
                return false;
            }
            "sessions" => self.sessions().await,
            "status" => self.status().await,
            "jobs" => self.jobs().await,
            "clear" => self.clear().await,
            "compact" => self.compact(argument).await,
            other => {
                if !other.is_empty() && other != "help" {
                    println!("  unknown command `/{other}`");
                }
                println!(
                    "  /new  /session  /sessions  /status  /jobs  /clear  /compact [N]  /quit"
                );
                return false;
            }
        };

        if let Err(error) = result {
            println!("  error: {error}");
        }
        false
    }

    /// Prints the sessions the host says this connection may act for.
    ///
    /// No filtering here, exactly as in a local run: the store already returned what the
    /// principal may act for, and a second filter would be a second place for the rule to be
    /// wrong.
    async fn sessions(&mut self) -> Result<()> {
        let Reply::Sessions(sessions) = self.call(Request::Sessions).await? else {
            return Err(Error::other("the host answered the wrong shape"));
        };
        if sessions.is_empty() {
            println!("  no sessions");
            return Ok(());
        }
        for stats in &sessions {
            let marker = if Some(stats.session) == self.session {
                "*"
            } else {
                " "
            };
            println!(
                "{marker} {}  {:>5} record(s)  ~{:>7} tokens  owner {}",
                stats.session, stats.records, stats.tokens, stats.owner,
            );
        }
        println!("  {} session(s); * is the current one", sessions.len());
        Ok(())
    }

    async fn status(&mut self) -> Result<()> {
        let Reply::Status(status) = self.call(Request::Status).await? else {
            return Err(Error::other("the host answered the wrong shape"));
        };
        println!("  host:   aikd {}", status.version);
        println!("  agent:  {} acting for {}", status.agent, status.user);
        println!("  model:  {}", status.model);
        println!("  root:   {}", status.root.display());
        match &status.database {
            Some(path) => println!("  store:  {}", path.display()),
            None => println!("  store:  none"),
        }
        println!("  memory: {}", status.memory);
        println!(
            "  jobs:   {}",
            if status.runs_jobs {
                "scheduled work runs in the host"
            } else {
                "the host runs no scheduled work"
            }
        );
        println!(
            "  up {}s, {} client(s) connected",
            status.uptime_ms / 1_000,
            status.connections,
        );
        Ok(())
    }

    async fn jobs(&mut self) -> Result<()> {
        let Reply::Jobs(jobs) = self.call(Request::Jobs).await? else {
            return Err(Error::other("the host answered the wrong shape"));
        };
        if jobs.is_empty() {
            println!("  no scheduled jobs");
            return Ok(());
        }
        for job in &jobs {
            println!(
                "  {}  owner {}  {}next {}",
                job.spec.id,
                job.owner,
                if job.spec.persistent {
                    "persistent  "
                } else {
                    "volatile    "
                },
                job.next_run
                    .map_or_else(|| "unknown".to_owned(), |at| at.as_millis().to_string()),
            );
        }
        println!("  {} job(s)", jobs.len());
        Ok(())
    }

    async fn clear(&mut self) -> Result<()> {
        let session = self.current()?;
        let Reply::Removed { records } = self.call(Request::Clear { session }).await? else {
            return Err(Error::other("the host answered the wrong shape"));
        };
        println!("  cleared session {session}: {records} record(s) removed");
        Ok(())
    }

    async fn compact(&mut self, argument: &str) -> Result<()> {
        let keep = match argument {
            "" => session::DEFAULT_COMPACT_KEEP,
            raw => raw.parse::<usize>().map_err(|_| {
                Error::InvalidArgument(format!(
                    "`/compact` takes the number of records to keep; `{raw}` is not one"
                ))
            })?,
        };
        let session = self.current()?;
        let Reply::Removed { records } = self.call(Request::Compact { session, keep }).await?
        else {
            return Err(Error::other("the host answered the wrong shape"));
        };
        println!("  compacted session {session}: {records} record(s) removed");
        Ok(())
    }

    /// The session these commands act on, or an error saying there is not one yet.
    fn current(&self) -> Result<SessionId> {
        self.session.ok_or_else(|| {
            Error::InvalidArgument(
                "this conversation has no session yet; ask something first".to_owned(),
            )
        })
    }

    /// Runs one prompt, printing updates and answering approvals as they arrive.
    async fn turn(&mut self, input: String) -> Result<()> {
        let id = self
            .client
            .send(Request::Prompt {
                session: self.session,
                input,
            })
            .await?;

        let mut stats = TurnStats::default();
        let mut interrupted = false;

        loop {
            let response = tokio::select! {
                biased;
                _ = tokio::signal::ctrl_c(), if !interrupted => {
                    interrupted = true;
                    println!("\n  interrupting…");
                    // The host cancels the call's own execution context, which is what
                    // reaches the model call and the tool call underneath it. Nothing is
                    // cancelled at this end, because nothing is running at this end.
                    self.client.send(Request::Cancel { call: id }).await?;
                    continue;
                }
                response = self.client.recv() => response?,
            };

            let Some(response) = response else {
                return Err(Error::other("the host closed the connection"));
            };

            match response {
                Response::Update {
                    id: answered,
                    update,
                } if answered == id => {
                    render::update(&update, &mut stats);
                }
                Response::Done {
                    id: answered,
                    reply,
                } if answered == id => {
                    if let Reply::Finished(response) = &reply {
                        // A resumed or newly started conversation: the host decides the id,
                        // and this end remembers whatever came back so `/clear`, `/compact`
                        // and the next turn name the same one.
                        self.session = Some(response.session);
                    }
                    return Ok(());
                }
                Response::Failed {
                    id: answered,
                    error,
                } if answered == id => {
                    return Err(error.into_error());
                }
                Response::Approval { pending } => self.answer(*pending).await?,
                Response::Closing { message } => {
                    return Err(Error::other(format!(
                        "the host is shutting down: {message}"
                    )));
                }
                // Another call's frame. There is only ever one turn in flight from this
                // terminal, so this is a host that answered an id nobody asked about.
                other => {
                    println!("  (ignoring an unexpected {other:?} from the host)");
                }
            }
        }
    }

    /// Puts one question to the person at the terminal and sends the answer back.
    async fn answer(&mut self, pending: aik_approval::PendingApproval) -> Result<()> {
        if !self.interactive {
            // The host only asks connections that said somebody is here. Reaching this means
            // it asked one that did not, and answering "no" is the safe reading.
            let _ = self
                .client
                .send(Request::Deny {
                    approval: pending.id,
                })
                .await?;
            return Ok(());
        }

        let reply = self
            .console
            .ask(&approval::question(&pending))
            .await
            .unwrap_or(None);
        let granted = approval::granted(reply.as_deref());
        println!("  {}", if granted { "allowed" } else { "denied" });

        let request = if granted {
            Request::Approve {
                approval: pending.id,
            }
        } else {
            Request::Deny {
                approval: pending.id,
            }
        };
        // Fire and forget the answer's own acknowledgement: a late answer is reported by the
        // host as a call that failed, and it is not worth ending the conversation over.
        self.client.send(request).await?;
        Ok(())
    }

    /// Sends one request and waits for its answer, answering any question that arrives first.
    async fn call(&mut self, request: Request) -> Result<Reply> {
        let id = self.client.send(request).await?;
        loop {
            let Some(response) = self.client.recv().await? else {
                return Err(Error::other("the host closed the connection"));
            };
            match response {
                Response::Done {
                    id: answered,
                    reply,
                } if answered == id => return Ok(reply),
                Response::Failed {
                    id: answered,
                    error,
                } if answered == id => {
                    return Err(error.into_error());
                }
                Response::Approval { pending } => self.answer(*pending).await?,
                Response::Closing { message } => {
                    return Err(Error::other(format!(
                        "the host is shutting down: {message}"
                    )));
                }
                Response::Update { .. } | Response::Done { .. } | Response::Failed { .. } => {}
            }
        }
    }
}

/// Reads the durable audit trail through a host process.
///
/// The database is locked by whichever process has it open, so a review while `aikd` runs has
/// to go through `aikd`. It reads under the same identity either way — the operator, not the
/// agent — because a socket in a `0700` directory establishes that the caller is the account
/// that owns the database, which is exactly what opening the file establishes.
pub async fn audit(socket: &Path, request: Request) -> Result<(Reply, PrincipalId)> {
    let endpoint = Endpoint::at(socket);
    // Not interactive: a review asks one question and prints the answer, and a console that
    // claimed somebody was present would make the host park approvals in front of it.
    let (mut client, connected) = Client::connect(&endpoint, &client_name(), false).await?;
    let expected = match &request {
        Request::Audit { .. } => "audit records",
        _ => "a prune result",
    };
    let reply = client.call(request).await?;
    match &reply {
        Reply::Audit { .. } | Reply::Pruned { .. } => Ok((reply, reader(&connected))),
        other => unexpected(expected, other),
    }
}

/// The identity a review through this host actually read as.
///
/// Taken from what the host reported rather than from anything typed here. The host's agent
/// acts *on behalf of* its operator, and that operator is who the trail was read as — so a
/// review reports the identity that decided what it could see, not the one the person running
/// the command happened to have configured locally.
fn reader(connected: &Connected) -> PrincipalId {
    connected
        .principal
        .on_behalf_of
        .clone()
        .unwrap_or_else(|| connected.principal.id.clone())
}
