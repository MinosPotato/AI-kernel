//! An agent context store: a durable transcript, and budgeted model windows derived from it.
//!
//! Every subsystem before this one was about *doing* something — calling a model, running a
//! tool, deciding whether that was allowed. This one is about what an agent knows while it
//! does those things, and what it has to pay, every turn, to keep knowing it.
//!
//! # The problem
//!
//! [`ModelProvider::complete`](aik_api::model::ModelProvider::complete) takes a
//! `Vec<Message>`. Nothing in the system holds one between calls, so an agent loop written
//! against that contract directly has exactly one option: keep the whole history in a local
//! vector and send all of it, every turn. That means
//!
//! * the same system prompt, the same early turns and the same tool output are re-sent on
//!   every request, so cost grows quadratically in turns;
//! * a single file read or directory listing is paid for once when it happens and then
//!   again on every subsequent turn for the rest of the conversation;
//! * and when the history outgrows the model's context window, there is no answer at all —
//!   the request simply starts failing.
//!
//! # The shape of the fix
//!
//! Stop treating the model payload as the place state lives.
//!
//! ```text
//!   append (trusted code only)          window (recomputed per turn)
//!        │                                        │
//!        ▼                                        ▼
//! ┌──────────────────────┐              ┌────────────────────────┐
//! │ ContextStore         │              │ ContextWindow          │
//! │  every record        │  ─ budget ─▶ │  the records that fit  │
//! │  full tool output    │              │  oversized parts elided│
//! │  who appended it     │              │  usage accounting      │
//! │  token accounting    │              └────────────────────────┘
//! └──────────────────────┘                          │
//!            ▲                                      ▼
//!            └── get(record) ────────────  CompletionRequest::messages
//! ```
//!
//! The store is the truth and it is never sent anywhere. The window is a projection of it,
//! computed fresh each turn under a [`ContextBudget`](aik_api::context::ContextBudget), and
//! thrown away afterwards. Anything the budget removed is still in the store, still
//! addressable by [`ContextRecord::id`](aik_api::context::ContextRecord::id), and named by
//! the marker left in its place — so eliding is bounded loss for the model, never loss for
//! the system.
//!
//! # What this crate contains
//!
//! * [`InMemoryContextStore`] — the reference [`ContextStore`](aik_api::context::ContextStore):
//!   append-only, per-session, owned, bounded.
//! * [`RedbContextStore`] — the same contract kept in the kernel's shared database, so a
//!   restart does not lose the conversation. Both run the same conformance tests.
//! * [`HeuristicTokenCounter`] — a provider-neutral size estimate, so budgeting works out of
//!   the box without the kernel acquiring a tokenizer.
//! * [`RetentionSweeper`] — the housekeeping half: removing sessions nobody came back to,
//!   off unless a component is told a retention period.
//! * [`ContextComponent`] and [`RedbContextComponent`] — the kernel wiring for each.
//!
//! # The lifecycle
//!
//! A durable transcript that could only be created and appended to was a store with no way
//! out of itself: sessions accumulated, none could be listed, none could be resumed by
//! anything that had lost the id, and one that reached
//! [`DEFAULT_MAX_RECORDS_PER_SESSION`] was permanently unusable. The contract now covers the
//! whole of it, and each step is deterministic and owner-scoped:
//!
//! ```text
//! create ──▶ append ──▶ enumerate ──▶ resume ──▶ compact ──▶ clear
//!  first      append()   sessions()    the id     compact()   clear()
//!  append                (filtered)    again      (bounded)   (whole)
//!                                                     │
//!                                         expire ◀────┘
//!                                      RetentionSweeper, on a timer
//! ```
//!
//! `sessions()` is the one step that filters rather than refusing — enumerating for a
//! principal must not become a way to learn that another principal's session exists. Every
//! step that *names* a session still fails closed with `PermissionDenied`. `compact()` is
//! what makes the record bound recoverable rather than terminal: it removes the oldest
//! unpinned records, never a pinned one, and appending resumes immediately afterwards.
//!
//! ```no_run
//! use aik_context::ContextComponent;
//! use aik_core::prelude::*;
//!
//! # fn build() -> Result<Kernel> {
//! Kernel::builder().component(ContextComponent::new()).build()
//! # }
//! ```
//!
//! A turn then looks like this — append what happened, ask for a window, send that:
//!
//! ```
//! use aik_api::context::{ContextBudget, ContextEntry, ContextStore};
//! use aik_api::execution::ExecutionContext;
//! use aik_api::model::{Message, Role};
//! use aik_api::agent::SessionId;
//! use aik_context::InMemoryContextStore;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> aik_core::Result<()> {
//! let store = InMemoryContextStore::new();
//! let session = SessionId::new();
//! let cx = ExecutionContext::new();
//!
//! store
//!     .append(
//!         &session,
//!         ContextEntry::new(Message::text(Role::System, "You are terse.")).pinned(),
//!         &cx,
//!     )
//!     .await?;
//! store
//!     .append(&session, ContextEntry::new(Message::text(Role::User, "hello")), &cx)
//!     .await?;
//!
//! // Never drop the pinned system prompt; elide any single part over 512 tokens.
//! let budget = ContextBudget::tokens(8_000).with_max_part_tokens(512);
//! let window = store.window(&session, &budget, &cx).await?;
//!
//! assert_eq!(window.messages.len(), 2);
//! assert_eq!(window.usage.dropped_records, 0);
//! # Ok(())
//! # }
//! ```
//!
//! # Security
//!
//! The transcript is the one piece of system state whose *contents* are largely written by
//! a model, so the boundary that matters is between what a model can influence and what it
//! cannot. It can influence the text of a record. It cannot influence:
//!
//! * **Who a record is attributed to.** The store stamps
//!   [`ContextRecord::principal`](aik_api::context::ContextRecord::principal) from the
//!   [`ExecutionContext`](aik_api::execution::ExecutionContext), not from the payload.
//! * **Where a record sits.** Session, sequence and timestamp are assigned by the store. The
//!   contract is append-only: there is no update and no insert-at, so history cannot be
//!   rewritten after the fact.
//! * **Whether it is forgettable.** Pinning is set by the trusted caller assembling the
//!   agent. A model that could pin its own output could make itself unforgettable.
//! * **Which sessions exist, or whose it can read.** A session is owned by the principal
//!   that created it; anyone else is refused, including the system principal.
//! * **Any of it, at all, directly.** A [`ContextStore`](aik_api::context::ContextStore) is
//!   not a [`Tool`](aik_api::tool::Tool) and must never be registered as one. There is no
//!   path from model output to this trait that does not go through trusted code deciding to
//!   record something.
//!
//! Nothing security-relevant is *kept* here either. Policy, authorization decisions and
//! approvals live in [`aik_api::permission`] and [`aik_api::audit`]; none of them is ever
//! round-tripped through a transcript, so writing to a context store cannot change what a
//! principal may do. The audit trail likewise stays whole regardless of what a budget
//! elides: it is published to the event bus as things happen and is not derived from the
//! window.
//!
//! One limit, stated plainly in the same spirit as the
//! [TOCTOU discussion](aik_api::tool#time-of-check-to-time-of-use): in-process code can
//! construct an `ExecutionContext` naming any principal, so session ownership is a boundary
//! against the model — which can never construct one — and defence in depth against a
//! confused caller. It is not a boundary against hostile code already inside the process.
//!
//! # What this deliberately does not do
//!
//! * **It does not summarise.** Replacing ten turns with a paragraph requires a model, which
//!   makes compaction a fallible, costly, non-deterministic operation with its own failure
//!   modes and its own prompt-injection surface. That belongs in a component above this one,
//!   which would read records through [`ContextStore`](aik_api::context::ContextStore) and
//!   append the summary back as an ordinary pinned record. Everything this crate does is
//!   deterministic and reproducible.
//! * **It does not deduplicate.** Collapsing a repeated file read into a back-reference is a
//!   real saving and a natural next step, but it changes what a model sees in a way that
//!   depends on assumptions about how models read history, and there is no reason to guess
//!   at those before there is an agent loop to measure.
//! * **It does not encrypt.** The file is the owner's alone (`0600` in a `0700` directory,
//!   enforced by [`aik_store`]) and nothing is written anywhere else, which is the part this
//!   crate can answer. Encryption at rest is a different threat model — one where the file
//!   itself is readable — and belongs above this crate rather than being invented here.
//! * **It does not expire anything unless asked.** [`RetentionSweeper`] exists and both
//!   stores implement it, but there is no default retention period: see
//!   [`retention`](crate::RetentionSweeper) for why the one switch in this crate that
//!   destroys data a user still has is the one switch that has to be turned on deliberately.
//! * **It does not compact tool schemas.** Tool descriptions are re-sent every turn too, but
//!   [`CompletionRequest::tools`](aik_api::model::CompletionRequest::tools) already carries
//!   names rather than full specifications, so that saving belongs at the provider boundary,
//!   not here.

mod component;
mod persistent;
mod retention;
mod session;
mod store;
mod tokens;
mod window;

pub use component::{ContextComponent, DEFAULT_COMPONENT_ID, RedbContextComponent};
pub use persistent::RedbContextStore;
pub use retention::{DEFAULT_RETENTION_BATCH, DEFAULT_RETENTION_SWEEP_INTERVAL, RetentionSweeper};
pub use store::{DEFAULT_MAX_RECORDS_PER_SESSION, InMemoryContextStore};
pub use tokens::{DEFAULT_BYTES_PER_TOKEN, HeuristicTokenCounter};
