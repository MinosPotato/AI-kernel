//! [`InMemoryContextStore`]: the reference [`ContextStore`] implementation.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use aik_api::agent::SessionId;
use aik_api::context::{
    ContextBudget, ContextEntry, ContextId, ContextRecord, ContextStats, ContextStore,
    ContextWindow, TokenCounter,
};
use aik_api::execution::ExecutionContext;
use aik_api::permission::PrincipalId;
use aik_core::clock::{SharedClock, SystemClock, Timestamp};
use aik_core::event::EventBus;
use aik_core::id::ComponentId;
use aik_core::{Error, Result};
use async_trait::async_trait;

use crate::session::{AssemblyReporter, authorize, principal_of};
use crate::tokens::HeuristicTokenCounter;
use crate::window::assemble;

/// The most records one session will hold before appends are refused.
///
/// A transcript grows under the direction of a model: every turn a model takes, and every
/// tool it calls, is another append. Without a ceiling, a loop that never terminates is a
/// memory-exhaustion bug in a process that also holds a policy engine and open file
/// handles. The bound is high enough that no legitimate conversation reaches it and low
/// enough that an unbounded one is stopped rather than tolerated.
pub const DEFAULT_MAX_RECORDS_PER_SESSION: usize = 10_000;

/// One session's records and who may see them.
struct SessionState {
    owner: PrincipalId,
    created_at: Timestamp,
    updated_at: Timestamp,
    next_sequence: u64,
    tokens: u64,
    records: Vec<ContextRecord>,
    index: HashMap<ContextId, usize>,
}

/// A [`ContextStore`] that keeps sessions in memory, in this process.
///
/// # What it guarantees
///
/// * **Append-only.** There is no update and no insert-at, so ordering is a property of the
///   store rather than of whoever wrote last.
/// * **Attributed by the store.** [`ContextRecord::principal`], `session`, `sequence` and
///   `created_at` come from the [`ExecutionContext`] and the kernel clock, never from the
///   appended [`ContextEntry`] — so nothing a model produced can influence them.
/// * **Session-scoped retrieval.** [`ContextStore::get`] resolves ids within one session
///   only; a valid id from another session is simply not found.
/// * **Owned sessions.** The principal that creates a session is the only one that may read
///   it, apart from principals acting explicitly on its behalf.
/// * **Bounded.** At most [`DEFAULT_MAX_RECORDS_PER_SESSION`] records per session by
///   default.
///
/// # What it does not
///
/// It does not persist. A restart loses every session, which is correct for the first
/// implementation — durable transcripts raise retention, encryption and deletion questions
/// that deserve their own answer rather than being decided implicitly by whichever database
/// got wired in first. The [`ContextStore`] contract is the seam a persistent
/// implementation slots into with nothing else changing.
pub struct InMemoryContextStore {
    sessions: RwLock<HashMap<SessionId, SessionState>>,
    counter: Arc<dyn TokenCounter>,
    clock: SharedClock,
    reporter: AssemblyReporter,
    max_records: usize,
}

impl std::fmt::Debug for InMemoryContextStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sessions = self.sessions.read().expect("context store lock poisoned");
        f.debug_struct("InMemoryContextStore")
            .field("sessions", &sessions.len())
            .field(
                "records",
                &sessions
                    .values()
                    .map(|session| session.records.len())
                    .sum::<usize>(),
            )
            .field("max_records_per_session", &self.max_records)
            .field("events_configured", &self.reporter.is_configured())
            .finish()
    }
}

impl Default for InMemoryContextStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryContextStore {
    /// Creates an empty store with the heuristic token counter and the system clock.
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            counter: Arc::new(HeuristicTokenCounter::new()),
            clock: Arc::new(SystemClock),
            reporter: AssemblyReporter::silent(ComponentId::new(crate::DEFAULT_COMPONENT_ID)),
            max_records: DEFAULT_MAX_RECORDS_PER_SESSION,
        }
    }

    /// Uses a different token counter.
    ///
    /// This is how a provider-specific tokenizer replaces the default estimate; see
    /// [`HeuristicTokenCounter`] for what that buys and when it matters.
    #[must_use]
    pub fn with_token_counter(mut self, counter: Arc<dyn TokenCounter>) -> Self {
        self.counter = counter;
        self
    }

    /// Overrides the clock used to stamp records. Defaults to the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Publishes [`ContextAssembled`](aik_api::context::ContextAssembled) events to the
    /// kernel event bus, attributed to `source`.
    ///
    /// Without a bus, windows are assembled identically and simply are not observable.
    #[must_use]
    pub fn with_events(mut self, events: EventBus, source: ComponentId) -> Self {
        self.reporter = AssemblyReporter::new(events, source);
        self
    }

    /// Overrides how many records one session may hold.
    #[must_use]
    pub fn with_max_records(mut self, max_records: usize) -> Self {
        self.max_records = max_records;
        self
    }

    /// The token counter in use, for a caller that needs to estimate before appending.
    pub fn token_counter(&self) -> &Arc<dyn TokenCounter> {
        &self.counter
    }
}

#[async_trait]
impl ContextStore for InMemoryContextStore {
    async fn append(
        &self,
        session: &SessionId,
        entry: ContextEntry,
        cx: &ExecutionContext,
    ) -> Result<ContextRecord> {
        let principal = principal_of(cx);
        let now = self.clock.now();
        // Counted outside the lock: an arbitrarily large message must not hold up every
        // other session while it is measured.
        let tokens = self.counter.count_message(&entry.message);

        let mut sessions = self.sessions.write().expect("context store lock poisoned");

        // Both checks happen before the session is created, so that a refused append leaves
        // no trace of the session it was refused from. The persistent store gets that for
        // free — its transaction aborts — and the two must not differ: a store where a
        // rejected first append conjures an empty owned session, and one where it does not,
        // are two different contracts however narrow the case that separates them.
        if let Some(state) = sessions.get(session) {
            authorize(session, &state.owner, &principal)?;
            if state.records.len() >= self.max_records {
                return Err(Error::other(format!(
                    "context session `{session}` is full at {} records; compact or clear it",
                    self.max_records
                )));
            }
        } else if self.max_records == 0 {
            return Err(Error::other(format!(
                "context session `{session}` is full at 0 records; compact or clear it"
            )));
        }

        let state = sessions.entry(*session).or_insert_with(|| SessionState {
            owner: principal.id.clone(),
            created_at: now,
            updated_at: now,
            next_sequence: 0,
            tokens: 0,
            records: Vec::new(),
            index: HashMap::new(),
        });

        let record = ContextRecord {
            id: ContextId::new(),
            session: *session,
            sequence: state.next_sequence,
            message: entry.message,
            pinned: entry.pinned,
            principal: principal.id,
            created_at: now,
            tokens,
        };

        state.next_sequence += 1;
        state.updated_at = now;
        state.tokens += tokens;
        state.index.insert(record.id, state.records.len());
        state.records.push(record.clone());

        Ok(record)
    }

    async fn get(
        &self,
        session: &SessionId,
        id: &ContextId,
        cx: &ExecutionContext,
    ) -> Result<Option<ContextRecord>> {
        let sessions = self.sessions.read().expect("context store lock poisoned");
        let Some(state) = sessions.get(session) else {
            return Ok(None);
        };
        authorize(session, &state.owner, &principal_of(cx))?;

        Ok(state
            .index
            .get(id)
            .and_then(|position| state.records.get(*position))
            .cloned())
    }

    async fn window(
        &self,
        session: &SessionId,
        budget: &ContextBudget,
        cx: &ExecutionContext,
    ) -> Result<ContextWindow> {
        let window = {
            let sessions = self.sessions.read().expect("context store lock poisoned");
            let Some(state) = sessions.get(session) else {
                return Ok(ContextWindow::empty());
            };
            authorize(session, &state.owner, &principal_of(cx))?;
            assemble(&state.records, budget, self.counter.as_ref())
        };

        self.reporter
            .report(cx, *session, self.clock.now(), window.usage);

        Ok(window)
    }

    async fn stats(
        &self,
        session: &SessionId,
        cx: &ExecutionContext,
    ) -> Result<Option<ContextStats>> {
        let sessions = self.sessions.read().expect("context store lock poisoned");
        let Some(state) = sessions.get(session) else {
            return Ok(None);
        };
        authorize(session, &state.owner, &principal_of(cx))?;

        Ok(Some(ContextStats {
            session: *session,
            owner: state.owner.clone(),
            records: state.records.len(),
            tokens: state.tokens,
            created_at: state.created_at,
            updated_at: state.updated_at,
        }))
    }

    async fn clear(&self, session: &SessionId, cx: &ExecutionContext) -> Result<usize> {
        let mut sessions = self.sessions.write().expect("context store lock poisoned");
        let Some(state) = sessions.get(session) else {
            return Ok(0);
        };
        authorize(session, &state.owner, &principal_of(cx))?;

        let removed = state.records.len();
        sessions.remove(session);
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_api::model::{Message, Role};
    use aik_api::permission::{Principal, PrincipalKind};
    use aik_core::ErrorKind;
    use aik_core::clock::ManualClock;

    fn user(id: &str) -> ExecutionContext {
        ExecutionContext::new().with_principal(Principal::new(id, PrincipalKind::User))
    }

    fn entry(body: &str) -> ContextEntry {
        ContextEntry::new(Message::text(Role::User, body))
    }

    #[tokio::test]
    async fn records_are_sequenced_and_attributed_by_the_store() {
        let clock = Arc::new(ManualClock::new(Timestamp::from_millis(1_000)));
        let store = InMemoryContextStore::new().with_clock(clock.clone());
        let session = SessionId::new();
        let cx = user("alice");

        let first = store.append(&session, entry("one"), &cx).await.unwrap();
        let second = store.append(&session, entry("two"), &cx).await.unwrap();

        assert_eq!(first.sequence, 0);
        assert_eq!(second.sequence, 1);
        assert_eq!(first.principal, PrincipalId::new("alice"));
        assert_eq!(first.session, session);
        assert_eq!(first.created_at, Timestamp::from_millis(1_000));
        assert_ne!(first.id, second.id);
    }

    #[tokio::test]
    async fn a_context_with_no_principal_is_the_system() {
        let store = InMemoryContextStore::new();
        let session = SessionId::new();

        let record = store
            .append(&session, entry("one"), &ExecutionContext::new())
            .await
            .unwrap();
        assert_eq!(record.principal, PrincipalId::new(Principal::SYSTEM));
    }

    #[tokio::test]
    async fn a_full_session_refuses_further_appends() {
        let store = InMemoryContextStore::new().with_max_records(2);
        let session = SessionId::new();
        let cx = user("alice");

        store.append(&session, entry("one"), &cx).await.unwrap();
        store.append(&session, entry("two"), &cx).await.unwrap();

        let error = store
            .append(&session, entry("three"), &cx)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(error.to_string().contains("full"), "{error}");
    }

    #[tokio::test]
    async fn stats_report_full_fidelity_totals() {
        let store = InMemoryContextStore::new();
        let session = SessionId::new();
        let cx = user("alice");

        let first = store.append(&session, entry("one"), &cx).await.unwrap();
        let second = store.append(&session, entry("two"), &cx).await.unwrap();

        let stats = store.stats(&session, &cx).await.unwrap().unwrap();
        assert_eq!(stats.records, 2);
        assert_eq!(stats.tokens, first.tokens + second.tokens);
        assert_eq!(stats.owner, PrincipalId::new("alice"));
    }

    #[tokio::test]
    async fn an_unknown_session_has_no_stats_and_an_empty_window() {
        let store = InMemoryContextStore::new();
        let session = SessionId::new();
        let cx = user("alice");

        assert!(store.stats(&session, &cx).await.unwrap().is_none());
        assert_eq!(
            store
                .window(&session, &ContextBudget::UNLIMITED, &cx)
                .await
                .unwrap(),
            ContextWindow::empty()
        );
        assert_eq!(store.clear(&session, &cx).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn clearing_removes_the_session() {
        let store = InMemoryContextStore::new();
        let session = SessionId::new();
        let cx = user("alice");

        let record = store.append(&session, entry("one"), &cx).await.unwrap();
        assert_eq!(store.clear(&session, &cx).await.unwrap(), 1);
        assert!(
            store
                .get(&session, &record.id, &cx)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.stats(&session, &cx).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_cleared_session_can_be_reclaimed_by_a_different_principal() {
        let store = InMemoryContextStore::new();
        let session = SessionId::new();

        store
            .append(&session, entry("one"), &user("alice"))
            .await
            .unwrap();
        store.clear(&session, &user("alice")).await.unwrap();

        let record = store
            .append(&session, entry("one"), &user("bob"))
            .await
            .unwrap();
        assert_eq!(record.principal, PrincipalId::new("bob"));
        assert_eq!(record.sequence, 0);
    }
}
