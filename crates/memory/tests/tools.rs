//! What the memory tools do to a store, run against both implementations.
//!
//! These suites are about the tools themselves: what they store, what they refuse, and whose
//! memory a call turns out to be. They call [`Tool::invoke`](aik_api::tool::Tool::invoke)
//! directly, because the registry that would normally be in front of them belongs to another
//! crate — that the registry is the only door, that policy is consulted at it, and that an
//! agent reaches all of this through a model, are asserted in the cross-subsystem suite.
//!
//! Every assertion that is not about durability runs against both backends. A tool that
//! behaved differently depending on which store it was bound to would defeat the point of
//! writing it against the contract.

mod support;

use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryId, MemoryQuery};
use aik_api::tool::Tool;
use aik_core::ErrorKind;
use aik_core::clock::Timestamp;
use serde_json::json;
use std::time::Duration;
use support::{Backend, ToolKernel, agent_for, invoke, output, user};

/// The argument names a `memory.query` spec offers a model.
fn offered_arguments(tool: &aik_memory::MemoryQueryTool) -> Vec<String> {
    let schema = tool.spec().input_schema;
    let mut names: Vec<String> = schema["properties"]
        .as_object()
        .expect("an object schema")
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

/// The id a `memory.put` outcome reported.
fn stored_id(output: &serde_json::Value) -> MemoryId {
    output["id"]
        .as_str()
        .expect("a put outcome names the record it stored")
        .parse()
        .expect("a well-formed record id")
}

/// Stores one memory as `principal` and returns its id.
async fn remember(
    kernel: &ToolKernel,
    principal: &str,
    kind: &str,
    content: serde_json::Value,
) -> MemoryId {
    let stored = output(
        &kernel.tools().put,
        json!({ "kind": kind, "content": content }),
        &user(principal),
    )
    .await;
    stored_id(&stored)
}

async fn a_memory_stored_through_the_tools_belongs_to_the_caller(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "preference", json!({ "theme": "dark" })).await;

    let record = kernel
        .store()
        .get(&id, &user("alice"))
        .await
        .expect("alice may read her own memory")
        .expect("the memory was stored");

    assert_eq!(record.owner.as_str(), "alice");
    assert_eq!(record.kind.as_str(), "preference");
    assert_eq!(record.content, json!({ "theme": "dark" }));
    // Stamped from the kernel clock, which the fixture stopped at the epoch, rather than
    // from the wall clock or from anything the caller supplied.
    assert_eq!(record.created_at, Timestamp::EPOCH);
    assert_eq!(record.expires_at, None);

    kernel.shutdown().await;
}

async fn the_same_principal_retrieves_its_memory_by_id_and_by_query(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "fact", json!("the sky is blue")).await;

    let fetched = output(
        &kernel.tools().get,
        json!({ "id": id.to_string() }),
        &user("alice"),
    )
    .await;
    assert_eq!(fetched["found"], json!(true));
    assert_eq!(fetched["record"]["content"], json!("the sky is blue"));
    assert_eq!(fetched["record"]["id"], json!(id.to_string()));

    let found = output(
        &kernel.tools().query,
        json!({ "kinds": ["fact"] }),
        &user("alice"),
    )
    .await;
    assert_eq!(found["count"], json!(1));
    assert_eq!(found["records"][0]["id"], json!(id.to_string()));

    // A kind nobody stored under matches nothing, rather than everything.
    let empty = output(
        &kernel.tools().query,
        json!({ "kinds": ["note"] }),
        &user("alice"),
    )
    .await;
    assert_eq!(empty["count"], json!(0));

    kernel.shutdown().await;
}

async fn a_rendered_memory_never_names_its_owner(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "fact", json!("x")).await;

    let fetched = output(
        &kernel.tools().get,
        json!({ "id": id.to_string() }),
        &user("alice"),
    )
    .await;
    let record = &fetched["record"];
    assert!(record.get("owner").is_none(), "{record}");
    assert!(record.get("embedding").is_none(), "{record}");

    let found = output(&kernel.tools().query, json!({}), &user("alice")).await;
    assert!(found["records"][0].get("owner").is_none(), "{found}");

    kernel.shutdown().await;
}

async fn another_principal_can_neither_read_nor_find_it(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "fact", json!("alice's secret")).await;

    // Naming the record is refused: mallory is told no, not that it is absent.
    let error = invoke(
        &kernel.tools().get,
        json!({ "id": id.to_string() }),
        &user("mallory"),
    )
    .await
    .expect_err("mallory may not read alice's memory");
    assert_eq!(error.kind(), ErrorKind::Permission);

    // Enumerating simply does not include it, because an error would confirm it exists.
    let found = output(&kernel.tools().query, json!({}), &user("mallory")).await;
    assert_eq!(found["count"], json!(0));

    // And deleting is refused rather than performed.
    let error = invoke(
        &kernel.tools().delete,
        json!({ "id": id.to_string() }),
        &user("mallory"),
    )
    .await
    .expect_err("mallory may not delete alice's memory");
    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(
        kernel
            .store()
            .get(&id, &user("alice"))
            .await
            .expect("alice may still read it")
            .is_some()
    );

    kernel.shutdown().await;
}

async fn a_delegate_works_within_the_owners_scope_without_taking_it(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "preference", json!({ "theme": "dark" })).await;
    let assistant = agent_for("assistant", "alice");

    // Delegated recall reaches the delegator's memories.
    let found = output(
        &kernel.tools().query,
        json!({ "kinds": ["preference"] }),
        &assistant,
    )
    .await;
    assert_eq!(found["count"], json!(1));
    assert_eq!(found["records"][0]["id"], json!(id.to_string()));

    // Revising one of them does not transfer it: the record keeps the owner it had.
    output(
        &kernel.tools().put,
        json!({ "kind": "preference", "content": { "theme": "light" }, "id": id.to_string() }),
        &assistant,
    )
    .await;
    let revised = kernel
        .store()
        .get(&id, &user("alice"))
        .await
        .expect("alice may read her own memory")
        .expect("the memory is still there");
    assert_eq!(revised.owner.as_str(), "alice");
    assert_eq!(revised.content, json!({ "theme": "light" }));

    // What the delegate stores for itself is its own, and delegation runs one way: alice
    // does not inherit her agent's memories.
    let own = output(
        &kernel.tools().put,
        json!({ "kind": "note", "content": "the agent's own note" }),
        &assistant,
    )
    .await;
    let own = stored_id(&own);
    assert_eq!(
        kernel
            .store()
            .get(&own, &assistant)
            .await
            .expect("the agent may read its own memory")
            .expect("it was stored")
            .owner
            .as_str(),
        "assistant"
    );
    let error = kernel
        .store()
        .get(&own, &user("alice"))
        .await
        .expect_err("alice may not read her agent's memory");
    assert_eq!(error.kind(), ErrorKind::Permission);

    kernel.shutdown().await;
}

async fn the_owner_cannot_be_chosen_through_the_arguments(backend: Backend) {
    let kernel = backend.open_tools().await;

    // There is no owner argument, in any spelling, and an unknown field is a refusal rather
    // than something quietly dropped.
    for arguments in [
        json!({ "kind": "fact", "content": "x", "owner": "alice" }),
        json!({ "kind": "fact", "content": "x", "principal": "alice" }),
        json!({ "kind": "fact", "content": "x", "on_behalf_of": "alice" }),
        json!({ "kind": "fact", "content": "x", "created_at": 0 }),
        json!({ "kind": "fact", "content": "x", "expires_at": 0 }),
    ] {
        let error = invoke(&kernel.tools().put, arguments.clone(), &user("mallory"))
            .await
            .expect_err("an invented field should be refused");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{arguments}");
    }

    // Nor through content or metadata, which are stored verbatim and mean nothing to the
    // ownership rule.
    let stored = output(
        &kernel.tools().put,
        json!({
            "kind": "fact",
            "content": { "owner": "alice" },
            "metadata": { "owner": "alice", "principal": "alice" }
        }),
        &user("mallory"),
    )
    .await;
    let id = stored_id(&stored);
    assert_eq!(
        kernel
            .store()
            .get(&id, &user("mallory"))
            .await
            .expect("mallory may read what mallory stored")
            .expect("it was stored")
            .owner
            .as_str(),
        "mallory"
    );
    let error = kernel
        .store()
        .get(&id, &user("alice"))
        .await
        .expect_err("the record is mallory's, whatever it says inside");
    assert_eq!(error.kind(), ErrorKind::Permission);

    kernel.shutdown().await;
}

async fn an_id_belonging_to_someone_else_cannot_be_overwritten(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "preference", json!({ "theme": "dark" })).await;

    let error = invoke(
        &kernel.tools().put,
        json!({ "kind": "preference", "content": { "theme": "mallory's" }, "id": id.to_string() }),
        &user("mallory"),
    )
    .await
    .expect_err("an upsert must not be able to take a record away from its owner");
    assert_eq!(error.kind(), ErrorKind::Permission);

    let record = kernel
        .store()
        .get(&id, &user("alice"))
        .await
        .expect("alice may read her own memory")
        .expect("it is still there");
    assert_eq!(record.owner.as_str(), "alice");
    assert_eq!(record.content, json!({ "theme": "dark" }));

    kernel.shutdown().await;
}

async fn a_memory_larger_than_the_limit_is_refused_without_being_stored(backend: Backend) {
    let kernel = backend.open_tools().await;
    let oversized = "x".repeat(aik_memory::tools::DEFAULT_MAX_RECORD_BYTES + 1);

    let outcome = invoke(
        &kernel.tools().put,
        json!({ "kind": "fact", "content": oversized }),
        &user("alice"),
    )
    .await
    .expect("an oversized memory is something the model can react to");
    assert!(outcome.is_error, "{outcome:?}");

    let stored = kernel
        .store()
        .query(&MemoryQuery::default(), &user("alice"))
        .await
        .expect("querying works");
    assert!(stored.is_empty(), "nothing should have been stored");

    kernel.shutdown().await;
}

async fn expiry_is_measured_from_the_kernel_clock(backend: Backend) {
    let kernel = backend.open_tools().await;
    let stored = output(
        &kernel.tools().put,
        json!({ "kind": "fact", "content": "for a minute", "ttl_seconds": 60 }),
        &user("alice"),
    )
    .await;
    assert_eq!(stored["expires_at"], json!(60_000));

    let found = output(&kernel.tools().query, json!({}), &user("alice")).await;
    assert_eq!(found["count"], json!(1));

    kernel.clock().advance(Duration::from_secs(61));
    let found = output(&kernel.tools().query, json!({}), &user("alice")).await;
    assert_eq!(
        found["count"],
        json!(0),
        "an expired memory is not recalled"
    );

    kernel.shutdown().await;
}

async fn a_lifetime_outside_the_representable_range_is_refused(backend: Backend) {
    let kernel = backend.open_tools().await;
    for ttl in [
        json!(0),
        json!(aik_memory::tools::MAX_TTL_SECONDS + 1),
        json!(u64::MAX),
    ] {
        let error = invoke(
            &kernel.tools().put,
            json!({ "kind": "fact", "content": "x", "ttl_seconds": ttl }),
            &user("alice"),
        )
        .await
        .expect_err("an unusable lifetime should be refused");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "ttl {ttl}");
    }
    kernel.shutdown().await;
}

async fn forgetting_reports_whether_there_was_anything_to_forget(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "fact", json!("transient")).await;

    let first = output(
        &kernel.tools().delete,
        json!({ "id": id.to_string() }),
        &user("alice"),
    )
    .await;
    assert_eq!(first["deleted"], json!(true));

    let second = output(
        &kernel.tools().delete,
        json!({ "id": id.to_string() }),
        &user("alice"),
    )
    .await;
    assert_eq!(second["deleted"], json!(false));

    let fetched = output(
        &kernel.tools().get,
        json!({ "id": id.to_string() }),
        &user("alice"),
    )
    .await;
    assert_eq!(fetched["found"], json!(false));

    kernel.shutdown().await;
}

async fn recall_is_capped_however_much_it_asks_for(backend: Backend) {
    let kernel = backend.open_tools().await;
    for index in 0..5 {
        remember(&kernel, "alice", "fact", json!(index)).await;
    }

    let asked = output(
        &kernel.tools().query,
        json!({ "limit": usize::MAX }),
        &user("alice"),
    )
    .await;
    assert_eq!(
        asked["limit"],
        json!(aik_memory::tools::DEFAULT_MAX_RESULTS),
        "the cap, not what was asked for"
    );
    assert_eq!(asked["count"], json!(5));

    let narrowed = output(&kernel.tools().query, json!({ "limit": 2 }), &user("alice")).await;
    assert_eq!(narrowed["count"], json!(2));

    kernel.shutdown().await;
}

async fn metadata_filters_match_exactly(backend: Backend) {
    let kernel = backend.open_tools().await;
    output(
        &kernel.tools().put,
        json!({ "kind": "fact", "content": "a", "metadata": { "subject": "coffee" } }),
        &user("alice"),
    )
    .await;
    output(
        &kernel.tools().put,
        json!({ "kind": "fact", "content": "b", "metadata": { "subject": "tea" } }),
        &user("alice"),
    )
    .await;

    let found = output(
        &kernel.tools().query,
        json!({ "metadata": { "subject": "coffee" } }),
        &user("alice"),
    )
    .await;
    assert_eq!(found["count"], json!(1));
    assert_eq!(found["records"][0]["content"], json!("a"));

    kernel.shutdown().await;
}

/// Every tool refuses `cx` for the same reason, with the same error kind, and touches
/// nothing on the way out.
async fn assert_every_tool_refuses(
    kernel: &ToolKernel,
    cx: &ExecutionContext,
    id: &MemoryId,
    expected: ErrorKind,
) {
    for (name, result) in [
        (
            "memory.put",
            invoke(
                &kernel.tools().put,
                json!({ "kind": "fact", "content": "too late" }),
                cx,
            )
            .await,
        ),
        (
            "memory.get",
            invoke(&kernel.tools().get, json!({ "id": id.to_string() }), cx).await,
        ),
        (
            "memory.query",
            invoke(&kernel.tools().query, json!({}), cx).await,
        ),
        (
            "memory.delete",
            invoke(&kernel.tools().delete, json!({ "id": id.to_string() }), cx).await,
        ),
    ] {
        match result {
            Ok(outcome) => panic!("`{name}` should refuse an expired call, got {outcome:?}"),
            Err(error) => assert_eq!(error.kind(), expected, "`{name}`"),
        }
    }
}

async fn a_call_whose_context_has_expired_touches_nothing(backend: Backend) {
    let kernel = backend.open_tools().await;
    let id = remember(&kernel, "alice", "fact", json!("already here")).await;

    let cancelled = user("alice");
    cancelled.cancellation.cancel();
    assert_every_tool_refuses(&kernel, &cancelled, &id, ErrorKind::Cancelled).await;

    // The fixture's clock is stopped at the epoch, so a deadline there has already passed.
    let overdue = user("alice").with_deadline(Timestamp::EPOCH);
    assert_every_tool_refuses(&kernel, &overdue, &id, ErrorKind::Timeout).await;

    let remaining = kernel
        .store()
        .query(&MemoryQuery::default(), &user("alice"))
        .await
        .expect("querying works");
    assert_eq!(
        remaining.len(),
        1,
        "nothing was written and nothing was deleted"
    );
    assert_eq!(remaining[0].record.id, id);

    kernel.shutdown().await;
}

crate::both_backends!(
    a_memory_stored_through_the_tools_belongs_to_the_caller,
    the_same_principal_retrieves_its_memory_by_id_and_by_query,
    a_rendered_memory_never_names_its_owner,
    another_principal_can_neither_read_nor_find_it,
    a_delegate_works_within_the_owners_scope_without_taking_it,
    the_owner_cannot_be_chosen_through_the_arguments,
    an_id_belonging_to_someone_else_cannot_be_overwritten,
    a_memory_larger_than_the_limit_is_refused_without_being_stored,
    expiry_is_measured_from_the_kernel_clock,
    a_lifetime_outside_the_representable_range_is_refused,
    forgetting_reports_whether_there_was_anything_to_forget,
    recall_is_capped_however_much_it_asks_for,
    metadata_filters_match_exactly,
    a_call_whose_context_has_expired_touches_nothing,
    without_an_embedding_model_no_search_is_offered_and_none_is_accepted,
    with_an_embedding_model_a_search_is_offered_and_ranks_by_meaning,
    a_search_still_obeys_the_kind_filter_and_the_result_cap,
    a_score_floor_narrows_a_search,
    a_search_scopes_to_the_caller_like_every_other_recall,
);

async fn without_an_embedding_model_no_search_is_offered_and_none_is_accepted(backend: Backend) {
    let kernel = backend.open_tools().await;
    let query = &kernel.tools().query;

    assert_eq!(offered_arguments(query), ["kinds", "limit", "metadata"]);
    assert!(
        !query.spec().description.contains("similar"),
        "a store that cannot search by meaning must not describe itself as able to"
    );

    remember(&kernel, "alice", "fact", json!("alice drinks tea")).await;
    let error = invoke(query, json!({ "text": "tea" }), &user("alice"))
        .await
        .expect_err("the argument does not exist for this store");
    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");

    // And an ordinary recall still works, so the refusal is about the argument and not about
    // the tool being broken.
    let listed = output(query, json!({}), &user("alice")).await;
    assert_eq!(listed["count"], 1);
    assert!(listed.get("scores").is_none(), "{listed}");

    kernel.shutdown().await;
}

async fn with_an_embedding_model_a_search_is_offered_and_ranks_by_meaning(backend: Backend) {
    let kernel = backend.open_semantic_tools().await;
    let query = &kernel.tools().query;

    assert_eq!(
        offered_arguments(query),
        ["kinds", "limit", "metadata", "min_score", "text"]
    );

    let tea = remember(&kernel, "alice", "fact", json!("alice drinks tea")).await;
    let coffee = remember(&kernel, "alice", "fact", json!("bob drinks coffee")).await;

    let found = output(query, json!({ "text": "tea" }), &user("alice")).await;
    assert_eq!(found["count"], 2);
    let ids: Vec<&str> = found["records"]
        .as_array()
        .expect("an array of records")
        .iter()
        .map(|record| record["id"].as_str().expect("an id"))
        .collect();
    // Stored second, so ranking by recency would have put it first.
    assert_eq!(ids, vec![tea.to_string(), coffee.to_string()]);

    let scores = found["scores"].as_array().expect("a score per record");
    assert_eq!(scores.len(), 2);
    assert!(
        scores[0].as_f64().expect("a number") > scores[1].as_f64().expect("a number"),
        "{scores:?}"
    );

    kernel.shutdown().await;
}

async fn a_search_still_obeys_the_kind_filter_and_the_result_cap(backend: Backend) {
    let kernel = backend.open_semantic_tools().await;
    let query = &kernel.tools().query;

    remember(&kernel, "alice", "fact", json!("alice drinks tea")).await;
    let preference = remember(&kernel, "alice", "preference", json!("alice drinks tea")).await;

    let found = output(
        query,
        json!({ "text": "tea", "kinds": ["preference"] }),
        &user("alice"),
    )
    .await;
    assert_eq!(found["count"], 1);
    assert_eq!(found["records"][0]["id"], preference.to_string());

    let capped = output(query, json!({ "text": "tea", "limit": 1 }), &user("alice")).await;
    assert_eq!(capped["count"], 1);
    assert_eq!(
        capped["scores"]
            .as_array()
            .expect("a score per record")
            .len(),
        1,
        "the scores must stay aligned with the records the limit kept"
    );

    kernel.shutdown().await;
}

async fn a_score_floor_narrows_a_search(backend: Backend) {
    let kernel = backend.open_semantic_tools().await;
    let query = &kernel.tools().query;

    let tea = remember(&kernel, "alice", "fact", json!("alice drinks tea")).await;
    remember(&kernel, "alice", "fact", json!("bob drinks coffee")).await;

    let found = output(
        query,
        json!({ "text": "tea", "min_score": 0.99 }),
        &user("alice"),
    )
    .await;
    assert_eq!(found["count"], 1);
    assert_eq!(found["records"][0]["id"], tea.to_string());

    for arguments in [
        json!({ "min_score": 0.5 }),
        json!({ "text": "tea", "min_score": 4.0 }),
    ] {
        let error = invoke(query, arguments.clone(), &user("alice"))
            .await
            .expect_err("{arguments} should be refused");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{arguments}");
    }

    kernel.shutdown().await;
}

/// Similarity ranks; it does not widen. Another principal's memories are no more reachable
/// through a search than through a listing.
async fn a_search_scopes_to_the_caller_like_every_other_recall(backend: Backend) {
    let kernel = backend.open_semantic_tools().await;
    let query = &kernel.tools().query;

    remember(&kernel, "alice", "fact", json!("alice drinks tea")).await;
    let found = output(query, json!({ "text": "tea" }), &user("bob")).await;
    assert_eq!(found["count"], 0, "{found}");

    kernel.shutdown().await;
}

/// A restart is where the two backends are *supposed* to differ, so it is the one suite
/// written against each of them explicitly rather than through the shared macro.
#[tokio::test]
async fn a_durable_memory_survives_a_restart_and_a_volatile_one_does_not() {
    let mut durable = Backend::Redb.open_tools().await;
    let id = remember(&durable, "alice", "preference", json!({ "theme": "dark" })).await;
    durable.restart().await;

    let fetched = output(
        &durable.tools().get,
        json!({ "id": id.to_string() }),
        &user("alice"),
    )
    .await;
    assert_eq!(fetched["found"], json!(true));
    assert_eq!(fetched["record"]["content"], json!({ "theme": "dark" }));
    assert_eq!(
        durable
            .store()
            .get(&id, &user("alice"))
            .await
            .expect("alice may read her own memory")
            .expect("it survived")
            .owner
            .as_str(),
        "alice",
        "ownership is part of what is durable"
    );
    // And it is still nobody else's after the restart.
    let error = invoke(
        &durable.tools().get,
        json!({ "id": id.to_string() }),
        &user("mallory"),
    )
    .await
    .expect_err("a restart does not reset who a memory belongs to");
    assert_eq!(error.kind(), ErrorKind::Permission);
    durable.shutdown().await;

    let mut volatile = Backend::Memory.open_tools().await;
    let id = remember(&volatile, "alice", "preference", json!({ "theme": "dark" })).await;
    volatile.restart().await;
    let fetched = output(
        &volatile.tools().get,
        json!({ "id": id.to_string() }),
        &user("alice"),
    )
    .await;
    assert_eq!(
        fetched["found"],
        json!(false),
        "an in-memory store is gone at the next start, by design"
    );
    volatile.shutdown().await;
}
