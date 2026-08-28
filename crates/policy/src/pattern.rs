//! String matching shared by every axis a [`PolicyRule`](crate::PolicyRule) can match on.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A simple, generic string matcher.
///
/// The same mechanism matches [`ActionId`](aik_api::permission::ActionId)s and
/// [`ResourceId`](aik_api::permission::ResourceId)s alike, since both are just opaque,
/// hierarchically-named strings as far as this crate is concerned — nothing here knows
/// that one of them might be a filesystem path and the other a namespaced action name.
///
/// Written as plain text in configuration:
///
/// * `"*"` matches anything — [`Pattern::Any`].
/// * Text ending in `*` matches everything starting with what comes before it —
///   [`Pattern::Prefix`]. This is a literal string prefix, not a path- or
///   namespace-boundary-aware one: to scope a prefix to a directory, include the
///   separator. `"/home/user/project/*"` matches `/home/user/project/notes.md` but not
///   `/home/user/project-secret/notes.md`; `"/home/user/project*"` would match both. The
///   same applies to action namespaces: prefer `"filesystem.*"` over `"filesystem*"`.
/// * Anything else matches only that exact string — [`Pattern::Exact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// Matches any value.
    Any,
    /// Matches values starting with this text.
    Prefix(String),
    /// Matches only this exact value.
    Exact(String),
}

impl Pattern {
    /// Parses a pattern from its textual configuration form.
    pub fn parse(text: &str) -> Self {
        if text == "*" {
            Self::Any
        } else if let Some(prefix) = text.strip_suffix('*') {
            Self::Prefix(prefix.to_owned())
        } else {
            Self::Exact(text.to_owned())
        }
    }

    /// Returns true if `value` matches this pattern.
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Prefix(prefix) => value.starts_with(prefix.as_str()),
            Self::Exact(exact) => value == exact,
        }
    }

    /// Returns true if this pattern can never usefully match anything, e.g. the empty
    /// exact string produced by an empty configuration field.
    ///
    /// Public because every configuration surface built on [`Pattern`] has to reject the
    /// same mistake, and a second copy of "what counts as an empty matcher" is a second
    /// thing to keep in step with this one.
    pub fn is_vacuous(&self) -> bool {
        matches!(self, Self::Exact(exact) if exact.is_empty())
    }
}

impl Default for Pattern {
    /// Matches any value — the natural default for an omitted matcher field.
    fn default() -> Self {
        Self::Any
    }
}

impl std::fmt::Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Prefix(prefix) => write!(f, "{prefix}*"),
            Self::Exact(exact) => f.write_str(exact),
        }
    }
}

impl Serialize for Pattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(Self::parse(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_star_is_any() {
        assert_eq!(Pattern::parse("*"), Pattern::Any);
        assert!(Pattern::Any.matches(""));
        assert!(Pattern::Any.matches("anything at all"));
    }

    #[test]
    fn a_trailing_star_is_a_prefix() {
        let pattern = Pattern::parse("/workspace/*");
        assert_eq!(pattern, Pattern::Prefix("/workspace/".into()));
        assert!(pattern.matches("/workspace/notes.md"));
        assert!(!pattern.matches("/workspace-secret/notes.md"));
        assert!(!pattern.matches("/workspace"));
    }

    #[test]
    fn no_star_is_an_exact_match() {
        let pattern = Pattern::parse("filesystem.read");
        assert_eq!(pattern, Pattern::Exact("filesystem.read".into()));
        assert!(pattern.matches("filesystem.read"));
        assert!(!pattern.matches("filesystem.readx"));
    }

    #[test]
    fn patterns_round_trip_through_json() {
        for text in ["*", "fs.*", "fs.read"] {
            let pattern = Pattern::parse(text);
            let json = serde_json::to_string(&pattern).unwrap();
            assert_eq!(json, format!("{text:?}"));
            let parsed: Pattern = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, pattern);
        }
    }

    #[test]
    fn an_empty_pattern_is_vacuous() {
        assert!(Pattern::parse("").is_vacuous());
        assert!(!Pattern::parse("*").is_vacuous());
        assert!(!Pattern::parse("a*").is_vacuous());
        assert!(!Pattern::parse("a").is_vacuous());
    }

    #[test]
    fn default_is_any() {
        assert_eq!(Pattern::default(), Pattern::Any);
    }
}
