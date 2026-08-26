//! Similarity ranking: the arithmetic, the text a record is embedded from, and the handle a
//! store holds onto a model.
//!
//! Kept apart from [`crate::query`] because the two answer different questions. `query` is
//! what both stores share so they cannot filter differently; this is what makes a store
//! *semantic* at all, and a store built without it behaves exactly as it did before.

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::memory::{MemoryMatch, MemoryRecord};
use aik_api::model::{Embedder, ModelId};
use aik_core::{Error, Result};

/// The model a store embeds with, and the thing that runs it.
///
/// A store is *given* this rather than resolving it, for the reason every other capability
/// in this workspace is injected: a store that could reach a registry could reach anything
/// in it. Without one, a store still ranks a caller-supplied
/// [`MemoryQuery::embedding`](aik_api::memory::MemoryQuery::embedding) — that needs no model
/// — and refuses only
/// [`MemoryQuery::text`](aik_api::memory::MemoryQuery::text).
#[derive(Clone)]
pub(crate) struct SemanticIndex {
    embedder: Arc<dyn Embedder>,
    model: ModelId,
}

impl std::fmt::Debug for SemanticIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticIndex")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl SemanticIndex {
    pub(crate) fn new(embedder: Arc<dyn Embedder>, model: ModelId) -> Self {
        Self { embedder, model }
    }

    /// Embeds one string, failing rather than returning nothing.
    ///
    /// A caller cannot tell "the model produced no vector" from "the batch came back short",
    /// so neither is allowed to reach it as an absence.
    pub(crate) async fn embed(&self, text: &str, cx: &ExecutionContext) -> Result<Vec<f32>> {
        let inputs = [text.to_owned()];
        let mut vectors = self.embedder.embed(&self.model, &inputs, cx).await?;
        if vectors.len() != 1 {
            return Err(Error::other(format!(
                "embedding with `{}` returned {} vectors for one input",
                self.model,
                vectors.len()
            )));
        }
        Ok(vectors.remove(0))
    }
}

/// The text a record is embedded from.
///
/// A JSON string is embedded as itself, because that is what a model was going to see
/// anyway and the quotes around it are not part of the memory. Anything else is embedded as
/// its compact JSON, which is deterministic for a given value — the same record embedded
/// twice produces the same input, so a re-embedding is a no-op rather than a drift.
///
/// The kind and metadata are deliberately left out. They are already exact filters applied
/// before ranking, and folding them into the vector would let a shared metadata value pull
/// two unrelated memories together.
pub(crate) fn embedding_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Cosine similarity, or `None` when the two vectors cannot meaningfully be compared.
///
/// Differing widths mean the two were produced by different models, and a number computed
/// across them would be a similarity between two things that were never in the same space.
/// A zero-magnitude vector has no direction to compare at all. Both are absences rather than
/// low scores, so a caller ranking on the result cannot mistake "incomparable" for "unlike".
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0f64;
    let mut norm_a = 0f64;
    let mut norm_b = 0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 || !dot.is_finite() {
        return None;
    }
    let similarity = dot / (norm_a.sqrt() * norm_b.sqrt());
    if !similarity.is_finite() {
        return None;
    }
    // Rounding in f64 can put an identical pair a hair outside the range the measure is
    // defined on, which would make a `min_score` of 1.0 behave differently for two records
    // that are in fact the same.
    Some(similarity.clamp(-1.0, 1.0) as f32)
}

/// Ranks candidates against a query vector, best first.
///
/// A record with no embedding, or one of a different width, is not a weak match — it is not
/// a match, because nothing was measured. Such records are absent from a semantic result
/// rather than appended to the end of it, where their position would imply a score they do
/// not have.
///
/// Ties are broken by recency and then by id, so a result is stable across calls and
/// identical between the two stores.
pub(crate) fn rank_by_similarity(
    candidates: Vec<MemoryRecord>,
    query: &[f32],
    min_score: Option<f32>,
    limit: Option<usize>,
) -> Vec<MemoryMatch> {
    let mut scored: Vec<(f32, MemoryRecord)> = candidates
        .into_iter()
        .filter_map(|record| {
            let embedding = record.embedding.as_deref()?;
            let score = cosine(query, embedding)?;
            match min_score {
                Some(minimum) if score < minimum => None,
                _ => Some((score, record)),
            }
        })
        .collect();

    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .partial_cmp(left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| right.id.cmp(&left.id))
    });
    if let Some(limit) = limit {
        scored.truncate(limit);
    }
    scored
        .into_iter()
        .map(|(score, record)| MemoryMatch {
            record,
            score: Some(score),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::clock::Timestamp;
    use serde_json::json;

    fn record(created_at_ms: u64, embedding: Option<Vec<f32>>) -> MemoryRecord {
        let mut record =
            MemoryRecord::new("fact", json!({}), Timestamp::from_millis(created_at_ms));
        record.embedding = embedding;
        record
    }

    #[test]
    fn a_string_is_embedded_as_itself_and_anything_else_as_its_json() {
        assert_eq!(embedding_text(&json!("alice likes tea")), "alice likes tea");
        assert_eq!(embedding_text(&json!({ "a": 1 })), r#"{"a":1}"#);
    }

    #[test]
    fn identical_vectors_score_one_and_opposite_ones_minus_one() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
        assert_eq!(cosine(&[1.0, 0.0], &[-1.0, 0.0]), Some(-1.0));
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
    }

    #[test]
    fn magnitude_does_not_change_the_score() {
        let small = cosine(&[1.0, 2.0], &[1.0, 2.0]).expect("comparable");
        let large = cosine(&[1.0, 2.0], &[100.0, 200.0]).expect("comparable");
        assert!((small - large).abs() < 1e-6, "{small} vs {large}");
    }

    #[test]
    fn incomparable_vectors_have_no_score_rather_than_a_low_one() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), None, "widths differ");
        assert_eq!(cosine(&[], &[]), None, "nothing to compare");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), None, "no direction");
        assert_eq!(cosine(&[f32::NAN, 0.0], &[1.0, 0.0]), None, "not a number");
    }

    #[test]
    fn ranking_is_best_first_and_carries_the_score() {
        let near = record(1, Some(vec![1.0, 0.1]));
        let far = record(2, Some(vec![-1.0, 0.0]));
        let matches = rank_by_similarity(vec![far.clone(), near.clone()], &[1.0, 0.0], None, None);
        assert_eq!(matches[0].record.id, near.id);
        assert_eq!(matches[1].record.id, far.id);
        assert!(matches.iter().all(|matched| matched.score.is_some()));
    }

    #[test]
    fn an_unembedded_record_is_absent_rather_than_last() {
        let embedded = record(1, Some(vec![1.0, 0.0]));
        let bare = record(2, None);
        let mismatched = record(3, Some(vec![1.0, 0.0, 0.0]));
        let matches = rank_by_similarity(
            vec![embedded.clone(), bare, mismatched],
            &[1.0, 0.0],
            None,
            None,
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].record.id, embedded.id);
    }

    #[test]
    fn min_score_drops_results_before_the_limit_is_applied() {
        let near = record(1, Some(vec![1.0, 0.0]));
        let far = record(2, Some(vec![-1.0, 0.0]));
        let matches = rank_by_similarity(vec![near.clone(), far], &[1.0, 0.0], Some(0.5), Some(10));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].record.id, near.id);
    }

    #[test]
    fn equal_scores_are_broken_by_recency_then_id() {
        let older = record(1, Some(vec![1.0, 0.0]));
        let newer = record(2, Some(vec![1.0, 0.0]));
        let matches =
            rank_by_similarity(vec![older.clone(), newer.clone()], &[1.0, 0.0], None, None);
        assert_eq!(matches[0].record.id, newer.id);
        assert_eq!(matches[1].record.id, older.id);
    }
}
