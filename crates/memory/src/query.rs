//! Query semantics shared by both [`MemoryStore`](aik_api::memory::MemoryStore)
//! implementations: one copy, so a durable store cannot rank or filter subtly differently
//! from the in-memory one it is meant to be indistinguishable from.

use aik_api::memory::{MemoryKind, MemoryMatch, MemoryQuery, MemoryRecord};
use aik_core::{Error, Result};

/// Refuses a query this implementation cannot honour, rather than silently dropping the
/// fields it does not understand.
///
/// `text` and `embedding` ask for similarity ranking, which needs an
/// [`Embedder`](aik_api::model::Embedder) and an index neither store has yet. `min_score` is
/// meaningless without a score to compare it against, so it is refused for the same reason
/// rather than being quietly ignored.
pub(crate) fn reject_unsupported(query: &MemoryQuery) -> Result<()> {
    if query.text.is_some() || query.embedding.is_some() || query.min_score.is_some() {
        return Err(Error::Unsupported(
            "semantic memory search (text, embedding or min_score) is not implemented yet; \
             query by kind and metadata instead"
                .to_owned(),
        ));
    }
    Ok(())
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
pub(crate) fn matches_metadata(record: &MemoryRecord, filter: &serde_json::Map<String, serde_json::Value>) -> bool {
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
    candidates.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    if let Some(limit) = limit {
        candidates.truncate(limit);
    }
    candidates
        .into_iter()
        .map(|record| MemoryMatch { record, score: None })
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

    #[test]
    fn text_embedding_and_min_score_are_all_refused() {
        assert!(
            reject_unsupported(&MemoryQuery {
                text: Some("x".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            reject_unsupported(&MemoryQuery {
                embedding: Some(vec![0.0]),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            reject_unsupported(&MemoryQuery {
                min_score: Some(0.5),
                ..Default::default()
            })
            .is_err()
        );
        assert!(reject_unsupported(&MemoryQuery::default()).is_ok());
    }

    #[test]
    fn requested_kinds_are_deduplicated_and_sorted() {
        let query = MemoryQuery {
            kinds: vec![MemoryKind::new("b"), MemoryKind::new("a"), MemoryKind::new("b")],
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
        assert!(matches_metadata(&record("fact", 1), &serde_json::Map::new()));
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
