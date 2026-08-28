//! The ceilings and the prices, as configuration.

use std::collections::BTreeMap;

use aik_api::quota::{QuotaDimension, UsageCharge};
use aik_core::config::Config;
use aik_core::{Error, Result};
use aik_policy::Pattern;
use serde::{Deserialize, Serialize};

use crate::period::QuotaPeriod;

/// What one model's tokens cost, in millionths of a currency unit per million tokens.
///
/// Two units are involved and both are deliberate. Prices are quoted per *million* tokens
/// because that is how every provider publishes them, so a rate can be transcribed rather
/// than converted; and they are quoted in *micros* because a price like `$3.00 / Mtok` is
/// then the exact integer `3_000_000`, with no floating point anywhere between an operator's
/// intention and an enforced ceiling.
///
/// The currency is never named. An operator prices models in whatever unit they are billed
/// in, and nothing here converts, compares across currencies, or knows what one is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelPrice {
    /// Micros per million input tokens.
    pub input_micros_per_million: u64,
    /// Micros per million output tokens.
    pub output_micros_per_million: u64,
}

impl ModelPrice {
    /// What a charge's tokens cost at this price, rounded down, saturating.
    ///
    /// Rounding down means a ledger never charges for tokens nobody spent. The error is at
    /// most one micro per turn, which is a millionth of a currency unit against a ceiling
    /// expressed in whole ones.
    pub fn cost_micros(&self, charge: &UsageCharge) -> u64 {
        // Saturating at every step, in the width that makes saturation unreachable for any
        // real price: a wrapped cost is a budget that empties into an unbounded one.
        let cost = (u128::from(charge.input_tokens) * u128::from(self.input_micros_per_million))
            .saturating_add(
                u128::from(charge.output_tokens) * u128::from(self.output_micros_per_million),
            );
        u64::try_from(cost / 1_000_000).unwrap_or(u64::MAX)
    }
}

/// One ceiling: whose, over what window, on what.
///
/// Every `max_*` field is optional and every one that is set is enforced independently, so a
/// rule can cap turns without caring about tokens, or cost without caring about either.
/// A rule that sets none of them is refused rather than ignored — see
/// [`QuotaRule::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaRule {
    /// Which identity this rule counts, as a [`Pattern`]: `*`, a prefix like `agent.*`, or
    /// an exact principal id. Matches every identity if omitted.
    ///
    /// The identities a piece of work is charged to are the acting principal's id and, when
    /// it is acting for somebody, theirs as well — see [`crate::LimitedQuotaGuard`]. So
    /// `{"subject": "alice"}` caps everything done *by or for* Alice, however many agents do
    /// it, and `{"subject": "scheduler"}` caps what the schedule spends on everybody's
    /// behalf together.
    #[serde(default)]
    pub subject: Pattern,

    /// How often this rule's counter starts again.
    ///
    /// Required, and deliberately so: "500 turns" is not a quota until it says over what.
    pub period: QuotaPeriod,

    /// The most model turns this subject may take in one window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u64>,
    /// The most tokens it may send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// The most tokens it may receive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// The most tokens in both directions together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// The most a window may cost, in micros, priced by [`QuotaDocument::prices`].
    ///
    /// Setting this makes an unpriced model unusable for this subject: see
    /// [`QuotaDocument::price`] for why that is a refusal rather than a free turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micros: Option<u64>,

    /// What this rule is for, for an operator reading it back. Never used in matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl QuotaRule {
    /// A rule capping `subject`'s model turns per `period`, and nothing else.
    pub fn turns(subject: &str, period: QuotaPeriod, max_turns: u64) -> Self {
        Self {
            subject: Pattern::parse(subject),
            period,
            max_turns: Some(max_turns),
            max_input_tokens: None,
            max_output_tokens: None,
            max_total_tokens: None,
            max_cost_micros: None,
            description: None,
        }
    }

    /// Every dimension this rule caps, with its ceiling.
    pub fn ceilings(&self) -> Vec<(QuotaDimension, u64)> {
        [
            (QuotaDimension::Turns, self.max_turns),
            (QuotaDimension::InputTokens, self.max_input_tokens),
            (QuotaDimension::OutputTokens, self.max_output_tokens),
            (QuotaDimension::TotalTokens, self.max_total_tokens),
            (QuotaDimension::CostMicros, self.max_cost_micros),
        ]
        .into_iter()
        .filter_map(|(dimension, limit)| limit.map(|limit| (dimension, limit)))
        .collect()
    }

    /// Rejects the mistakes deserialisation cannot catch.
    ///
    /// A zero ceiling is one of them. It reads as "this subject may take no turns at all",
    /// which is not a budget but a prohibition, and a prohibition belongs in the policy
    /// document where it is auditable as an authorization decision — a quota refusal is not
    /// one. Nobody writes a zero on purpose here; it is a truncated number or an empty
    /// template field, and taking it literally would silence a deployment.
    pub fn validate(&self) -> Result<()> {
        if self.subject.is_vacuous() {
            return Err(Error::config(
                "subject",
                "an empty subject matches nothing; use `*` for every principal",
            ));
        }
        let ceilings = self.ceilings();
        if ceilings.is_empty() {
            return Err(Error::config(
                "",
                "this rule sets no ceiling; give it at least one of max_turns, \
                 max_input_tokens, max_output_tokens, max_total_tokens or max_cost_micros",
            ));
        }
        for (dimension, limit) in ceilings {
            if limit == 0 {
                return Err(Error::config(
                    key_for(dimension),
                    format!(
                        "a ceiling of zero {dimension} would refuse every request from this \
                         subject; deny the action in the policy document instead"
                    ),
                ));
            }
        }
        if self
            .description
            .as_ref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err(Error::config(
                "description",
                "a blank description says nothing",
            ));
        }
        Ok(())
    }
}

/// The configuration key a dimension's ceiling is written under.
fn key_for(dimension: QuotaDimension) -> &'static str {
    match dimension {
        QuotaDimension::Turns => "max_turns",
        QuotaDimension::InputTokens => "max_input_tokens",
        QuotaDimension::OutputTokens => "max_output_tokens",
        QuotaDimension::TotalTokens => "max_total_tokens",
        QuotaDimension::CostMicros => "max_cost_micros",
    }
}

/// Every ceiling a deployment sets, and the prices they are measured with.
///
/// # Every matching rule applies
///
/// Unlike [`PolicyDocument`](aik_policy::PolicyDocument), this is not first-match-wins.
/// Every rule whose subject matches is enforced, and the check refuses if any one of them is
/// exhausted. Order is therefore insignificant, and adding a rule can only ever tighten what
/// a deployment permits — which is the property worth having in the document that decides
/// how much can be spent. Raising one subject's ceiling is expressed by narrowing the rule
/// that constrains it, not by adding a more specific rule that overrides it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QuotaDocument {
    /// The ceilings. All of them apply.
    pub limits: Vec<QuotaRule>,
    /// What each model's tokens cost, keyed by a [`Pattern`] over the model id.
    pub prices: BTreeMap<String, ModelPrice>,
}

impl QuotaDocument {
    /// A document with no ceilings, which limits nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether this document constrains anything at all.
    ///
    /// A deployment with an empty document does no ledger writes and takes no locks: the
    /// guard is not merely permissive, it is inert. That is what makes a quota something a
    /// deployment opts into rather than a cost every deployment pays.
    pub fn is_empty(&self) -> bool {
        self.limits.is_empty()
    }

    /// Reads and validates a document from `config` at `path`, defaulting to an empty one.
    pub fn from_config(config: &Config, path: &str) -> Result<Self> {
        let document: Self = config.get_or_default(path)?;
        document.validate_at(path)?;
        Ok(document)
    }

    /// Checks every rule and every price, naming what is wrong and where.
    pub fn validate(&self) -> Result<()> {
        self.validate_at("")
    }

    fn validate_at(&self, section: &str) -> Result<()> {
        for (index, rule) in self.limits.iter().enumerate() {
            rule.validate()
                .map_err(|error| prefix(error, &join(section, &format!("limits[{index}]"))))?;
        }
        for model in self.prices.keys() {
            if Pattern::parse(model).is_vacuous() {
                return Err(Error::config(
                    join(section, "prices"),
                    "a price keyed by the empty string matches no model; use `*` for every model",
                ));
            }
        }
        Ok(())
    }

    /// What `model` costs, or `None` if the deployment has not priced it.
    ///
    /// An exact key wins; failing that, the longest matching prefix pattern does, so
    /// `"claude-*"` can price a family while `"claude-opus-4"` prices one member differently.
    /// Longest-prefix is a total order over the keys that match, so the answer never depends
    /// on map iteration order.
    ///
    /// # Why an unpriced model is not free
    ///
    /// Returning zero for a model nobody priced would make a cost ceiling unenforceable
    /// exactly when it matters — the day a deployment starts using a model whose price was
    /// never written down. So a cost ceiling over an unpriced model is a refusal
    /// ([`LimitedQuotaGuard::check`](crate::LimitedQuotaGuard)), naming the model and the key
    /// to add. A deployment that genuinely runs a free model says so with a price of zero.
    pub fn price(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(price) = self.prices.get(model) {
            return Some(price);
        }
        self.prices
            .iter()
            .filter_map(|(key, price)| match Pattern::parse(key) {
                Pattern::Any => Some((0usize, price)),
                Pattern::Prefix(prefix) if model.starts_with(&prefix) => {
                    Some((prefix.len() + 1, price))
                }
                _ => None,
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, price)| price)
    }
}

/// Rewrites a config error's path so it names the rule it came from.
fn prefix(error: Error, path: &str) -> Error {
    match error {
        Error::Config {
            path: inner,
            message,
        } => Error::config(join(path, &inner), message),
        other => other,
    }
}

fn join(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right.to_owned(),
        (_, true) => left.to_owned(),
        _ => format!("{left}.{right}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;
    use serde_json::json;

    fn document(value: serde_json::Value) -> Result<QuotaDocument> {
        QuotaDocument::from_config(
            &Config::builder().layer(json!({ "quota": value })).build(),
            "quota",
        )
    }

    #[test]
    fn an_absent_section_limits_nothing() {
        let document = QuotaDocument::from_config(&Config::builder().build(), "quota").unwrap();
        assert!(document.is_empty());
    }

    #[test]
    fn a_rule_reads_from_json() {
        let document = document(json!({
            "limits": [
                { "subject": "alice", "period": "day", "max_turns": 500,
                  "max_cost_micros": 5_000_000, "description": "one person's day" }
            ],
            "prices": { "claude-*": { "input_micros_per_million": 3_000_000,
                                      "output_micros_per_million": 15_000_000 } }
        }))
        .unwrap();

        let rule = &document.limits[0];
        assert_eq!(rule.subject, Pattern::Exact("alice".into()));
        assert_eq!(rule.period, QuotaPeriod::Day);
        assert_eq!(
            rule.ceilings(),
            vec![
                (QuotaDimension::Turns, 500),
                (QuotaDimension::CostMicros, 5_000_000)
            ]
        );
        assert!(!document.is_empty());
    }

    #[test]
    fn a_rule_with_no_ceiling_is_refused() {
        let error = document(json!({ "limits": [{ "period": "day" }] })).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(error.to_string().contains("quota.limits[0]"), "{error}");
        assert!(error.to_string().contains("max_turns"), "{error}");
    }

    #[test]
    fn a_zero_ceiling_is_refused_rather_than_silencing_a_deployment() {
        let error = document(json!({
            "limits": [{ "period": "day", "max_turns": 0 }]
        }))
        .unwrap_err();
        assert!(error.to_string().contains("max_turns"), "{error}");
        assert!(error.to_string().contains("policy document"), "{error}");
    }

    #[test]
    fn an_empty_subject_is_refused() {
        let error = document(json!({
            "limits": [{ "subject": "", "period": "day", "max_turns": 5 }]
        }))
        .unwrap_err();
        assert!(error.to_string().contains("subject"), "{error}");
    }

    #[test]
    fn a_missing_period_is_refused() {
        let error = document(json!({ "limits": [{ "max_turns": 5 }] })).unwrap_err();
        assert!(error.to_string().contains("period"), "{error}");
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        // A misspelled ceiling that silently did nothing would leave an operator believing
        // they had capped something they had not.
        let error = document(json!({
            "limits": [{ "period": "day", "max_turnz": 5 }]
        }))
        .unwrap_err();
        assert!(error.to_string().contains("max_turnz"), "{error}");
    }

    #[test]
    fn an_exact_price_beats_a_prefix_and_a_longer_prefix_beats_a_shorter_one() {
        let document = document(json!({
            "limits": [{ "period": "day", "max_turns": 1 }],
            "prices": {
                "*": { "input_micros_per_million": 1 },
                "claude-": { "input_micros_per_million": 2 },
                "claude-opus*": { "input_micros_per_million": 3 },
                "claude-opus-5": { "input_micros_per_million": 4 }
            }
        }))
        .unwrap();

        assert_eq!(
            document
                .price("claude-opus-5")
                .unwrap()
                .input_micros_per_million,
            4
        );
        assert_eq!(
            document
                .price("claude-opus-4")
                .unwrap()
                .input_micros_per_million,
            3
        );
        assert_eq!(
            document
                .price("claude-sonnet")
                .unwrap()
                .input_micros_per_million,
            1
        );
        assert_eq!(
            document
                .price("llama3.1:8b")
                .unwrap()
                .input_micros_per_million,
            1
        );
    }

    #[test]
    fn an_unpriced_model_has_no_price_rather_than_a_free_one() {
        let document = document(json!({
            "limits": [{ "period": "day", "max_turns": 1 }],
            "prices": { "claude-*": { "input_micros_per_million": 1 } }
        }))
        .unwrap();
        assert!(document.price("llama3.1:8b").is_none());
    }

    #[test]
    fn a_price_multiplies_per_million_tokens_and_rounds_down() {
        let price = ModelPrice {
            input_micros_per_million: 3_000_000,
            output_micros_per_million: 15_000_000,
        };
        // 1M in, 100k out: 3.00 + 1.50 currency units.
        let charge = UsageCharge::turn("m", 1_000_000, 100_000);
        assert_eq!(price.cost_micros(&charge), 4_500_000);

        // A single token of a $3/Mtok model is three micros, rounded down from 3.0.
        assert_eq!(price.cost_micros(&UsageCharge::turn("m", 1, 0)), 3);
        // And a fraction of a micro is never charged.
        let cheap = ModelPrice {
            input_micros_per_million: 1,
            output_micros_per_million: 0,
        };
        assert_eq!(cheap.cost_micros(&UsageCharge::turn("m", 999_999, 0)), 0);
    }

    #[test]
    fn a_price_saturates_rather_than_overflowing() {
        let price = ModelPrice {
            input_micros_per_million: u64::MAX,
            output_micros_per_million: u64::MAX,
        };
        assert_eq!(
            price.cost_micros(&UsageCharge::turn("m", u64::MAX, u64::MAX)),
            u64::MAX
        );
    }

    #[test]
    fn a_price_keyed_by_nothing_is_refused() {
        let error = document(json!({
            "limits": [{ "period": "day", "max_turns": 1 }],
            "prices": { "": { "input_micros_per_million": 1 } }
        }))
        .unwrap_err();
        assert!(error.to_string().contains("prices"), "{error}");
    }
}
