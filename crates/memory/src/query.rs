//! Query semantics shared by both [`MemoryStore`](aik_api::memory::MemoryStore)
//! implementations: one copy, so a durable store cannot rank or filter subtly differently
//! from the in-memory one it is meant to be indistinguishable from.

use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryKind, MemoryMatch, MemoryQuery, MemoryRecord};
use aik_core::{Error, Result};

use crate::semantic::SemanticIndex;

/// How a query is to be answered, once its fields have been reconciled with what the store
/// can actually do.
///
/// Resolving this *before* any record is read is what keeps the decision in one place: both
/// stores ask the same question, get the same answer, and cannot end up ranking differently
/// because one of them checked a field the other forgot.
#[derive(Debug)]
pub(crate) enum QueryMode {
    /// Filter only. Ordering is most recent first and no result carries a score.
    Exact,
    /// Rank against this vector, dropping anything below `min_score`.
    Semantic {
        /// What to compare stored embeddings against.
        vector: Vec<f32>,
        /// The floor a result must clear, if the caller set one.
        min_score: Option<f32>,
    },
}

/// Decides how to answer a query, embedding its text if it has any.
///
/// Three rules, each of which refuses rather than degrades:
///
/// * A `min_score` with nothing to score is [`Error::Unsupported`]. A floor on a number that
///   was never computed cannot be applied, and applying it to nothing would silently return
///   everything.
/// * `text` and `embedding` together is [`Error::InvalidArgument`]: they are two different
///   questions, and picking one would answer the one the caller did not ask.
/// * `text` without a [`SemanticIndex`] is [`Error::Unsupported`], never a fallback to
///   recency. A store that answered a search with "here is the newest thing I have" would
///   read as a memory that had forgotten, rather than as one that cannot search.
pub(crate) async fn resolve_mode(
    query: &MemoryQuery,
    semantic: Option<&SemanticIndex>,
    cx: &ExecutionContext,
) -> Result<QueryMode> {
    if query.text.is_some() && query.embedding.is_some() {
        return Err(Error::InvalidArgument(
            "a memory query carries both `text` and `embedding`; supply one or the other"
                .to_owned(),
        ));
    }

    let vector = match (&query.embedding, &query.text) {
        (Some(embedding), _) => {
            if embedding.is_empty() {
                return Err(Error::InvalidArgument(
                    "a memory query's `embedding` is empty".to_owned(),
                ));
            }
            Some(embedding.clone())
        }
        (None, Some(text)) => {
            let index = semantic.ok_or_else(|| {
                Error::Unsupported(
                    "this memory store has no embedding model, so it cannot search by `text`; \
                     supply a pre-computed `embedding`, or query by kind and metadata"
                        .to_owned(),
                )
            })?;
            Some(index.embed(text, cx).await?)
        }
        (None, None) => None,
    };

    match vector {
        Some(vector) => Ok(QueryMode::Semantic {
            vector,
            min_score: query.min_score,
        }),
        None if query.min_score.is_some() => Err(Error::Unsupported(
            "`min_score` needs something to score; supply `text` or `embedding` with it".to_owned(),
        )),
        None => Ok(QueryMode::Exact),
    }
}

/// Applies whichever ordering the resolved mode calls for.
pub(crate) fn rank_for(
    mode: &QueryMode,
    candidates: Vec<MemoryRecord>,
    limit: Option<usize>,
) -> Vec<MemoryMatch> {
    match mode {
        QueryMode::Exact => rank(candidates, limit),
        QueryMode::Semantic { vector, min_score } => {
            crate::semantic::rank_by_similarity(candidates, vector, *min_score, limit)
        }
    }
}

/// The kinds a query asks for, deduplicated so a store scanning a per-kind index does not
/// visit — and so does not return — the same kind twice.
pub(crate) fn requested_kinds(query: &MemoryQuery) -> Vec<MemoryKind> {
    let mut kinds = query.kinds.clone();
    kinds.sort();
    kinds.dedup();
    kinds
}

/// Whether every key the query names is present in the record's metadata with an equal
/// value. An empty filter matches everything.
pub(crate) fn matches_metadata(
    record: &MemoryRecord,
    filter: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    filter
        .iter()
        .all(|(key, value)| record.metadata.get(key) == Some(value))
}

/// Orders candidates best-first and applies the query's limit.
///
/// Neither store computes a similarity score — see the crate's top-level documentation for
/// why — so "best" falls back to the most recently created record, tie-broken by id so the
/// order is deterministic even for records created in the same millisecond.
pub(crate) fn rank(mut candidates: Vec<MemoryRecord>, limit: Option<usize>) -> Vec<MemoryMatch> {
    candidates.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    if let Some(limit) = limit {
        candidates.truncate(limit);
    }
    candidates
        .into_iter()
        .map(|record| MemoryMatch {
            record,
            score: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::Timestamp;
    use serde_json::json;

    fn record(kind: &str, created_at_ms: u64) -> MemoryRecord {
        MemoryRecord::new(kind, json!({}), Timestamp::from_millis(created_at_ms))
    }

    #[tokio::test]
    async fn a_query_with_no_similarity_fields_is_exact() {
        let mode = resolve_mode(&MemoryQuery::default(), None, &ExecutionContext::new())
            .await
            .expect("nothing to embed");
        assert!(matches!(mode, QueryMode::Exact));
    }

    #[tokio::test]
    async fn a_precomputed_embedding_needs_no_model() {
        let query = MemoryQuery {
            embedding: Some(vec![1.0, 0.0]),
            min_score: Some(0.25),
            ..Default::default()
        };
        let mode = resolve_mode(&query, None, &ExecutionContext::new())
            .await
            .expect("arithmetic only");
        match mode {
            QueryMode::Semantic { vector, min_score } => {
                assert_eq!(vector, vec![1.0, 0.0]);
                assert_eq!(min_score, Some(0.25));
            }
            QueryMode::Exact => panic!("an embedding asks for ranking"),
        }
    }

    #[tokio::test]
    async fn text_without_an_embedding_model_is_unsupported_not_a_fallback() {
        let query = MemoryQuery {
            text: Some("what does alice like?".into()),
            ..Default::default()
        };
        let error = resolve_mode(&query, None, &ExecutionContext::new())
            .await
            .expect_err("no model is configured");
        assert_eq!(error.kind(), aik_core::ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn min_score_alone_is_refused() {
        let query = MemoryQuery {
            min_score: Some(0.5),
            ..Default::default()
        };
        let error = resolve_mode(&query, None, &ExecutionContext::new())
            .await
            .expect_err("nothing to score");
        assert_eq!(error.kind(), aik_core::ErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn text_and_embedding_together_are_refused() {
        let query = MemoryQuery {
            text: Some("x".into()),
            embedding: Some(vec![1.0]),
            ..Default::default()
        };
        let error = resolve_mode(&query, None, &ExecutionContext::new())
            .await
            .expect_err("two different questions");
        assert_eq!(error.kind(), aik_core::ErrorKind::InvalidArgument);
    }

    #[tokio::test]
    async fn an_empty_embedding_is_refused() {
        let query = MemoryQuery {
            embedding: Some(Vec::new()),
            ..Default::default()
        };
        let error = resolve_mode(&query, None, &ExecutionContext::new())
            .await
            .expect_err("nothing to compare against");
        assert_eq!(error.kind(), aik_core::ErrorKind::InvalidArgument);
    }

    #[test]
    fn requested_kinds_are_deduplicated_and_sorted() {
        let query = MemoryQuery {
            kinds: vec![
                MemoryKind::new("b"),
                MemoryKind::new("a"),
                MemoryKind::new("b"),
            ],
            ..Default::default()
        };
        assert_eq!(
            requested_kinds(&query),
            vec![MemoryKind::new("a"), MemoryKind::new("b")]
        );
    }

    #[test]
    fn metadata_filter_requires_every_key_to_match_exactly() {
        let mut record = record("fact", 1);
        record.metadata.insert("source".into(), json!("chat"));
        record.metadata.insert("confidence".into(), json!(0.9));

        let mut filter = serde_json::Map::new();
        filter.insert("source".into(), json!("chat"));
        assert!(matches_metadata(&record, &filter));

        filter.insert("confidence".into(), json!(0.1));
        assert!(!matches_metadata(&record, &filter));
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        assert!(matches_metadata(
            &record("fact", 1),
            &serde_json::Map::new()
        ));
    }

    #[test]
    fn ranking_is_most_recent_first_tie_broken_by_id() {
        let older = record("fact", 1);
        let newer = record("fact", 2);
        let matches = rank(vec![older.clone(), newer.clone()], None);
        assert_eq!(matches[0].record.id, newer.id);
        assert_eq!(matches[1].record.id, older.id);
        assert!(matches.iter().all(|m| m.score.is_none()));
    }

    #[test]
    fn a_limit_truncates_after_ranking() {
        let records = vec![record("fact", 1), record("fact", 2), record("fact", 3)];
        let matches = rank(records, Some(2));
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].record.created_at, Timestamp::from_millis(3));
        assert_eq!(matches[1].record.created_at, Timestamp::from_millis(2));
    }
}
