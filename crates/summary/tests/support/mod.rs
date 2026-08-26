//! A scriptable model and a real context store, which is everything a compactor needs.

// A shared test module is compiled into every integration test binary, so anything one of
// them does not use looks dead, and nothing in a test binary is reachable from outside it.
#![allow(dead_code, unreachable_pub)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aik_api::agent::SessionId;
use aik_api::context::{ContextEntry, ContextStore};
use aik_api::execution::ExecutionContext;
use aik_api::model::{
    CompletionChunk, CompletionRequest, CompletionResponse, ContentPart, FinishReason, Message,
    ModelDescriptor, ModelProvider, Role,
};
use aik_api::permission::{Principal, PrincipalKind};
use aik_context::InMemoryContextStore;
use aik_core::{Error, Result};
use async_trait::async_trait;
use futures_core::stream::BoxStream;

/// What the scripted model does when it is asked for a recap.
#[derive(Clone)]
pub enum Answer {
    /// Replies with this text.
    Text(String),
    /// Replies with a message carrying no text at all.
    Silent,
    /// Fails.
    Failure(String),
}

/// A model that answers from a script and keeps every request it was sent.
pub struct ScriptedModel {
    answers: Vec<Answer>,
    calls: AtomicUsize,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedModel {
    pub fn new(answers: impl IntoIterator<Item = Answer>) -> Arc<Self> {
        Arc::new(Self {
            answers: answers.into_iter().collect(),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// A model that replies with the same recap however often it is asked.
    pub fn saying(text: &str) -> Arc<Self> {
        Self::new(std::iter::repeat_n(Answer::Text(text.to_owned()), 8))
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("request lock").clone()
    }

    /// The excerpt sent on call `index`, zero-based.
    pub fn excerpt(&self, index: usize) -> String {
        let request = self
            .requests()
            .get(index)
            .cloned()
            .unwrap_or_else(|| panic!("no summarisation request {index}"));
        request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl ModelProvider for ScriptedModel {
    async fn models(&self) -> Result<Vec<ModelDescriptor>> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<CompletionResponse> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().expect("request lock").push(request);

        let answer = self
            .answers
            .get(index)
            .cloned()
            .unwrap_or_else(|| Answer::Failure(format!("the script has no answer {index}")));

        match answer {
            Answer::Text(text) => Ok(CompletionResponse {
                message: Message::text(Role::Assistant, text),
                finish_reason: FinishReason::Stop,
                usage: None,
            }),
            Answer::Silent => Ok(CompletionResponse {
                message: Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                    name: None,
                },
                finish_reason: FinishReason::Stop,
                usage: None,
            }),
            Answer::Failure(reason) => Err(Error::other(reason)),
        }
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
        _cx: &ExecutionContext,
    ) -> Result<BoxStream<'static, Result<CompletionChunk>>> {
        Err(Error::Unsupported(
            "the scripted model does not stream".into(),
        ))
    }
}

/// The principal every test acts as unless it is testing that it cannot.
pub fn alice() -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new("alice", PrincipalKind::User))
}

/// Somebody else entirely.
pub fn mallory() -> ExecutionContext {
    ExecutionContext::new().with_principal(Principal::new("mallory", PrincipalKind::User))
}

/// A store holding `turns` alternating user/assistant turns, newest last.
pub async fn conversation(
    turns: usize,
    cx: &ExecutionContext,
) -> (Arc<InMemoryContextStore>, SessionId) {
    let store = Arc::new(InMemoryContextStore::new());
    let session = SessionId::new();
    for index in 0..turns {
        let role = if index % 2 == 0 {
            Role::User
        } else {
            Role::Assistant
        };
        store
            .append(
                &session,
                ContextEntry::new(Message::text(role, format!("turn {index}"))),
                cx,
            )
            .await
            .expect("appending a turn");
    }
    (store, session)
}

/// Every record of a session, oldest first, read back at full fidelity.
pub async fn records(
    store: &Arc<InMemoryContextStore>,
    session: &SessionId,
    cx: &ExecutionContext,
) -> Vec<aik_api::context::ContextRecord> {
    let window = store
        .window(session, &aik_api::context::ContextBudget::UNLIMITED, cx)
        .await
        .expect("a window");
    let mut records = Vec::new();
    for id in window.records {
        if let Some(record) = store.get(session, &id, cx).await.expect("a record") {
            records.push(record);
        }
    }
    records
}

/// The text of a record, however many parts it has.
pub fn text_of(record: &aik_api::context::ContextRecord) -> String {
    record
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
