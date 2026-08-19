//! The kernel error type.
//!
//! One error enum covers the whole kernel. It is `#[non_exhaustive]`, so variants can be
//! added without a breaking change, and callers that need to branch on failures should
//! match on [`ErrorKind`] rather than on the variants themselves.
//!
//! Subsystems built on the kernel are expected to wrap their own errors with
//! [`Error::wrap`] rather than to define a parallel error hierarchy.

use std::time::Duration;

use crate::id::ComponentId;

/// A boxed, thread-safe source error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The kernel result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The lifecycle phase a component failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecyclePhase {
    /// `Component::init`.
    Init,
    /// `Component::start`.
    Start,
    /// `Component::stop`.
    Stop,
}

impl std::fmt::Display for LifecyclePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Init => "init",
            Self::Start => "start",
            Self::Stop => "stop",
        })
    }
}

/// A coarse classification of failures, stable across variant additions.
///
/// Use this instead of matching on [`Error`] directly when deciding whether to retry,
/// report or escalate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The configuration is missing or malformed.
    Config,
    /// A lookup failed.
    NotFound,
    /// A registration conflicts with an existing one.
    Conflict,
    /// The wiring of components or services is invalid.
    Wiring,
    /// A lifecycle operation was invalid or failed.
    Lifecycle,
    /// The caller supplied something invalid.
    InvalidArgument,
    /// The operation is not supported by this implementation.
    Unsupported,
    /// The operation was refused by policy.
    Permission,
    /// A resource resolved outside a boundary the caller enforces on itself, independently
    /// of any authorization decision — e.g. a symlink escaping a confined root.
    ///
    /// Distinct from [`ErrorKind::Permission`]: nothing was asked and refused, so this is not
    /// an authorization outcome. Kept distinct from [`ErrorKind::InvalidArgument`] too, even
    /// though both originate from validating input, so a consumer of this classification can
    /// alert on an actual boundary violation without also matching every malformed request.
    Confinement,
    /// The operation ran out of time.
    Timeout,
    /// The operation was cancelled.
    Cancelled,
    /// Data could not be encoded or decoded.
    Serialization,
    /// Anything else.
    Other,
}

/// The error type returned throughout the kernel.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Configuration was missing or could not be interpreted.
    #[error("configuration error at `{path}`: {message}")]
    Config {
        /// The dotted configuration path involved.
        path: String,
        /// What was wrong with it.
        message: String,
    },

    /// A lookup failed.
    #[error("no {kind} named `{id}`")]
    NotFound {
        /// What kind of thing was looked up, e.g. `component` or `service`.
        kind: &'static str,
        /// The identifier that was looked up.
        id: String,
    },

    /// Something is already registered under that identity.
    #[error("a {kind} named `{id}` is already registered")]
    AlreadyExists {
        /// What kind of thing was being registered.
        kind: &'static str,
        /// The conflicting identifier.
        id: String,
    },

    /// A service was requested by type, but several are registered and none is the default.
    #[error("service `{service}` is ambiguous; candidates: {candidates:?}")]
    Ambiguous {
        /// The requested service type.
        service: &'static str,
        /// The registered candidates.
        candidates: Vec<String>,
    },

    /// A component declared a required dependency that was never registered.
    #[error("component `{component}` requires `{dependency}`, which is not registered")]
    MissingDependency {
        /// The component with the unmet requirement.
        component: ComponentId,
        /// The dependency it asked for.
        dependency: ComponentId,
    },

    /// The component dependency graph contains a cycle.
    #[error("dependency cycle among components: {0:?}")]
    DependencyCycle(Vec<ComponentId>),

    /// A lifecycle operation was requested in a state that does not allow it.
    #[error("invalid lifecycle transition: {0}")]
    Lifecycle(String),

    /// A component failed during a lifecycle phase.
    #[error("component `{component}` failed during {phase}")]
    Component {
        /// The failing component.
        component: ComponentId,
        /// The phase it failed in.
        phase: LifecyclePhase,
        /// The underlying failure.
        #[source]
        source: BoxError,
    },

    /// The caller supplied an invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The implementation does not support this operation.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The operation was refused by policy.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// A resource resolved outside a boundary enforced independently of policy.
    ///
    /// See [`ErrorKind::Confinement`].
    #[error("confinement violation: {0}")]
    Confinement(String),

    /// The operation exceeded its time budget.
    #[error("operation timed out after {0:?}")]
    Timeout(Duration),

    /// The operation was cancelled, typically by shutdown.
    #[error("operation was cancelled")]
    Cancelled,

    /// Data could not be encoded or decoded.
    #[error("serialization error")]
    Serialization(#[source] serde_json::Error),

    /// Anything that does not fit the variants above.
    #[error("{context}")]
    Other {
        /// A description of what was being attempted.
        context: String,
        /// The underlying failure, if any.
        #[source]
        source: Option<BoxError>,
    },
}

impl Error {
    /// Creates an [`Error::Other`] from a message.
    pub fn other(context: impl Into<String>) -> Self {
        Self::Other {
            context: context.into(),
            source: None,
        }
    }

    /// Wraps a foreign error with a description of what was being attempted.
    ///
    /// This is the intended way for subsystems to surface their own errors through the
    /// kernel without defining a parallel error hierarchy.
    pub fn wrap(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Other {
            context: context.into(),
            source: Some(source.into()),
        }
    }

    /// Creates a [`Error::NotFound`].
    pub fn not_found(kind: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            kind,
            id: id.to_string(),
        }
    }

    /// Creates an [`Error::AlreadyExists`].
    pub fn already_exists(kind: &'static str, id: impl std::fmt::Display) -> Self {
        Self::AlreadyExists {
            kind,
            id: id.to_string(),
        }
    }

    /// Creates an [`Error::Config`].
    pub fn config(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Config {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Classifies the error.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Config { .. } => ErrorKind::Config,
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::AlreadyExists { .. } => ErrorKind::Conflict,
            Self::Ambiguous { .. } | Self::MissingDependency { .. } | Self::DependencyCycle(_) => {
                ErrorKind::Wiring
            }
            Self::Lifecycle(_) | Self::Component { .. } => ErrorKind::Lifecycle,
            Self::InvalidArgument(_) => ErrorKind::InvalidArgument,
            Self::Unsupported(_) => ErrorKind::Unsupported,
            Self::PermissionDenied(_) => ErrorKind::Permission,
            Self::Confinement(_) => ErrorKind::Confinement,
            Self::Timeout(_) => ErrorKind::Timeout,
            Self::Cancelled => ErrorKind::Cancelled,
            Self::Serialization(_) => ErrorKind::Serialization,
            Self::Other { .. } => ErrorKind::Other,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confinement_is_classified_distinctly_from_invalid_argument_and_permission() {
        let error = Error::Confinement("path resolves outside the tool's allowed root".into());
        assert_eq!(error.kind(), ErrorKind::Confinement);
        assert_ne!(error.kind(), ErrorKind::InvalidArgument);
        assert_ne!(error.kind(), ErrorKind::Permission);
        assert_eq!(
            error.to_string(),
            "confinement violation: path resolves outside the tool's allowed root"
        );
    }

    #[test]
    fn wrapped_errors_keep_their_source() {
        let inner = std::io::Error::other("disk on fire");
        let error = Error::wrap("loading the model catalogue", inner);
        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(error.to_string(), "loading the model catalogue");
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn component_failures_are_attributed() {
        let error = Error::Component {
            component: ComponentId::new("platform.hyprland"),
            phase: LifecyclePhase::Start,
            source: Box::new(Error::other("no compositor")),
        };
        assert_eq!(
            error.to_string(),
            "component `platform.hyprland` failed during start"
        );
        assert_eq!(error.kind(), ErrorKind::Lifecycle);
    }
}
