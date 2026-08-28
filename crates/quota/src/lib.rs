//! Cumulative spend ceilings for the AI kernel.
//!
//! `aik-agent` already bounds one run — so many model turns, so many tool calls, a window of
//! so many tokens — and those bounds are exactly right for what they are: they stop one
//! conversation from running away. They reset with the run. Nothing in them stops the same
//! principal from starting the next one, and nothing at all stops a
//! [scheduled](aik_api::scheduler) job from starting one every hour until somebody notices
//! the bill.
//!
//! This crate is the ceiling that does not reset. It implements
//! [`QuotaGuard`](aik_api::quota::QuotaGuard) from a document of rules read through the
//! kernel's ordinary [`Config`](aik_core::Config) mechanism, over a ledger that — in the
//! durable backend — outlives the process it constrains.
//!
//! # The pieces
//!
//! * [`QuotaRule`] — one ceiling: whose, over what window, on what.
//! * [`QuotaPeriod`] — the window a ceiling is measured over, in UTC.
//! * [`ModelPrice`] — what a model's tokens cost, so a ceiling can be money rather than
//!   tokens.
//! * [`QuotaDocument`] — the validated set of rules and prices, as configuration.
//! * [`UsageLedger`] — where counters live: [`InMemoryUsageLedger`] or [`RedbUsageLedger`].
//! * [`LimitedQuotaGuard`] — the enforcement itself.
//! * [`QuotaComponent`] / [`RedbQuotaComponent`] — either of those, as a kernel component.
//!
//! # A complete document
//!
//! ```
//! use aik_core::config::Config;
//! use aik_quota::QuotaDocument;
//! use serde_json::json;
//!
//! let config = Config::builder()
//!     .layer(json!({
//!         "quota": {
//!             "limits": [
//!                 { "subject": "*", "period": "day", "max_turns": 500,
//!                   "description": "a day's work for anybody" },
//!                 { "subject": "*", "period": "month", "max_cost_micros": 50_000_000,
//!                   "description": "50 currency units a month, all in" },
//!                 { "subject": "scheduler", "period": "hour", "max_turns": 20,
//!                   "description": "autonomous work, unattended" }
//!             ],
//!             "prices": {
//!                 "claude-*": { "input_micros_per_million": 3_000_000,
//!                               "output_micros_per_million": 15_000_000 },
//!                 "*": { "input_micros_per_million": 0, "output_micros_per_million": 0 }
//!             }
//!         }
//!     }))
//!     .build();
//!
//! let document = QuotaDocument::from_config(&config, "quota").unwrap();
//! assert_eq!(document.limits.len(), 3);
//! ```
//!
//! Every rule whose subject matches applies, so the three above are all in force at once and
//! the tightest of them is what stops a run. That is the opposite of
//! [`aik-policy`](aik_policy)'s first-match-wins, and deliberately: a policy document decides
//! *whether* something may happen, where an override is often what an operator means, while
//! this one decides *how much*, where an addition should never widen what came before it.
//!
//! # What it does not do
//!
//! It does not authorize anything. A principal with budget left still needs a policy rule
//! allowing what it is about to do, and a principal with a policy rule allowing everything
//! still stops when its budget is gone. The two are independent gates on the same run.
//!
//! It does not bill, meter for anyone else, or report. `aik-audit` is the record
//! of what happened and
//! [`RequestMeasured`](aik_api::measurement::RequestMeasured) is the per-turn cost as it
//! happens; a ledger row here is a counter that enforcement reads, kept only while its window
//! is open.
//!
//! # Where it is enforced
//!
//! In the agent loop, which is the only place that both knows a turn is about to be taken and
//! finds out what it cost. A deployment with no guard registered behaves exactly as it did
//! before this crate existed.

mod component;
mod document;
mod guard;
mod ledger;
mod period;
mod persistent;

pub use component::{DEFAULT_COMPONENT_ID, QuotaComponent, RedbQuotaComponent};
pub use document::{ModelPrice, QuotaDocument, QuotaRule};
pub use guard::LimitedQuotaGuard;
pub use ledger::{Counters, InMemoryUsageLedger, UsageLedger};
pub use period::{QuotaPeriod, Window};
pub use persistent::RedbUsageLedger;
