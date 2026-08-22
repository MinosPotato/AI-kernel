//! Configuration for the audit trail.
//!
//! One setting, and it is the only one that destroys data, so it is the only one worth
//! putting in a configuration file rather than in code: how long records are kept.

use std::time::Duration;

use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// Settings read from the audit component's configuration section.
///
/// With no configuration at all, nothing is ever removed — see
/// [`crate::retention`](crate::retention) for why that is the right default for this
/// particular collection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditSettings {
    /// How many days of trail to keep. Absent means keep everything, for ever.
    ///
    /// Counted in days rather than seconds because that is the unit the decision is actually
    /// made in — "ninety days" is a retention policy, "7776000" is a transcription error
    /// waiting to happen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,

    /// How often the retention sweep runs, in seconds. Absent means
    /// [`DEFAULT_RETENTION_SWEEP_INTERVAL`](crate::DEFAULT_RETENTION_SWEEP_INTERVAL).
    ///
    /// Does nothing on its own: with no `retention_days` there is no sweep to schedule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep_interval_seconds: Option<u64>,
}

impl AuditSettings {
    /// The configured retention period, if any.
    ///
    /// A period of zero days is refused rather than taken literally: it would mean "delete
    /// every record as soon as the next sweep notices it", which nobody configures on
    /// purpose and which would empty the trail of a running system. Someone who genuinely
    /// wants no trail turns the component off.
    pub fn retention(&self, section: &str) -> Result<Option<Duration>> {
        match self.retention_days {
            None => Ok(None),
            Some(0) => Err(Error::config(
                format!("components.{section}.retention_days"),
                "a retention period of zero days would discard the audit trail as fast as it \
                 is written; remove the audit component instead of configuring this",
            )),
            Some(days) => Ok(Some(Duration::from_secs(u64::from(days) * 24 * 60 * 60))),
        }
    }

    /// The configured sweep interval, if any.
    ///
    /// Zero is refused for the same class of reason: a sweep every no-time is a task that
    /// never yields the write slot.
    pub fn sweep_interval(&self, section: &str) -> Result<Option<Duration>> {
        match self.sweep_interval_seconds {
            None => Ok(None),
            Some(0) => Err(Error::config(
                format!("components.{section}.sweep_interval_seconds"),
                "a sweep interval of zero seconds would never yield the database's write slot",
            )),
            Some(seconds) => Ok(Some(Duration::from_secs(seconds))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    #[test]
    fn nothing_configured_keeps_everything_for_ever() {
        let settings = AuditSettings::default();
        assert_eq!(settings.retention("audit.store").unwrap(), None);
        assert_eq!(settings.sweep_interval("audit.store").unwrap(), None);
    }

    #[test]
    fn a_retention_period_is_counted_in_days() {
        let settings = AuditSettings {
            retention_days: Some(90),
            ..AuditSettings::default()
        };
        assert_eq!(
            settings.retention("audit.store").unwrap(),
            Some(Duration::from_secs(90 * 24 * 60 * 60))
        );
    }

    #[test]
    fn a_zero_retention_period_is_refused_rather_than_emptying_the_trail() {
        let settings = AuditSettings {
            retention_days: Some(0),
            ..AuditSettings::default()
        };
        let error = settings.retention("audit.store").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(error.to_string().contains("retention_days"), "{error}");
    }

    #[test]
    fn a_zero_sweep_interval_is_refused() {
        let settings = AuditSettings {
            sweep_interval_seconds: Some(0),
            ..AuditSettings::default()
        };
        let error = settings.sweep_interval("audit.store").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        // A misspelled `retention_days` that silently did nothing would leave an operator
        // believing they had configured a retention policy they had not.
        let error =
            serde_json::from_value::<AuditSettings>(serde_json::json!({"retention_dayz": 30}))
                .unwrap_err();
        assert!(error.to_string().contains("retention_dayz"), "{error}");
    }
}
