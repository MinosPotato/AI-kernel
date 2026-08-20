//! Mapping [`redb`]'s error types onto [`aik_core::Error`].
//!
//! The kernel has one error enum and expects subsystems to wrap their own failures into it
//! rather than define a parallel hierarchy — see [`aik_core::Error::wrap`]. This module is
//! that wrapping, done once, so no other code in the workspace ever matches on a redb
//! error.
//!
//! # Why the classification matters
//!
//! Callers branch on [`ErrorKind`](aik_core::ErrorKind), not on variants, and the kind is
//! what decides whether a failure is retried, reported to an operator, or treated as fatal.
//! Collapsing every storage failure into `Other` would be accurate but useless, so the
//! mapping promotes the failures whose *handling* differs:
//!
//! | redb failure | Kernel error | Kind | Why |
//! |---|---|---|---|
//! | `DatabaseAlreadyOpen` | `AlreadyExists` | `Conflict` | Another process holds the file lock: a conflicting exclusive claim, not a malformed input. Reported by [`open_error`], which knows the path. |
//! | `UpgradeRequired` | `Config` | `Config` | The file predates this redb major version and needs an operator-run upgrade. |
//! | `TableTypeMismatch`, `TypeDefinitionChanged`, `TableIsMultimap`, `TableIsNotMultimap` | `Config` | `Config` | The on-disk schema disagrees with this binary — the same class of problem as a version mismatch, and never resolvable by retrying. |
//! | `TableDoesNotExist` | `NotFound` | `NotFound` | An ordinary lookup miss. |
//! | `TableExists`, `TableAlreadyOpen` | `AlreadyExists` | `Conflict` | A conflicting registration. |
//! | `ValueTooLarge` | `InvalidArgument` | `InvalidArgument` | The caller supplied something oversized. |
//! | everything else | `Other` | `Other` | I/O, corruption, poisoned locks, misuse of this crate. |
//!
//! `Corrupted` is deliberately *not* promoted to a distinct kind. Doing so would mean
//! extending `ErrorKind` in `aik-core`, and no caller yet branches on it; what matters
//! about corruption is that it must never be retried, which `Other` already implies. The
//! message says `the database is corrupted` so an operator reading a log is not left
//! guessing.

use std::path::Path;

use aik_core::Error;

/// The configuration path named by the [`Error::Config`] failures in this module.
///
/// A schema or file-format mismatch is resolved by pointing the store at a different file,
/// or upgrading the one it has, so the operator-facing handle for it is the setting that
/// chooses the file. A store registered under a non-default component id has the same
/// setting under its own `components.<id>` prefix.
const SETTING: &str = "components.store.db.path";

/// Converts any redb error into a kernel error, describing what was being attempted.
///
/// `context` is the same kind of phrase [`Error::wrap`] takes throughout the workspace: a
/// present participle naming the operation, e.g. `"reading the schema version"`.
///
/// ```
/// use aik_core::ErrorKind;
/// use aik_store::store_error;
///
/// let error = store_error(
///     "opening the meta table",
///     redb::TableError::TableDoesNotExist("aik.meta".to_owned()),
/// );
/// assert_eq!(error.kind(), ErrorKind::NotFound);
/// ```
pub fn store_error(context: &str, error: impl Into<redb::Error>) -> Error {
    let error = error.into();
    match error {
        redb::Error::DatabaseAlreadyOpen => Error::AlreadyExists {
            kind: "exclusive claim on the database file",
            id: context.to_owned(),
        },
        redb::Error::UpgradeRequired(version) => Error::config(
            SETTING,
            format!(
                "the database is in redb file format v{version} and must be upgraded manually \
                 before this build can open it",
            ),
        ),
        redb::Error::TableTypeMismatch { ref table, .. }
        | redb::Error::TableIsMultimap(ref table)
        | redb::Error::TableIsNotMultimap(ref table) => Error::config(
            SETTING,
            format!(
                "table `{table}` does not have the shape this build expects, so the database \
                 was written by an incompatible binary",
            ),
        ),
        redb::Error::TypeDefinitionChanged { ref name, .. } => Error::config(
            SETTING,
            format!(
                "the stored definition of type `{name:?}` differs from this build's, so the \
                 database was written by an incompatible binary",
            ),
        ),
        redb::Error::TableDoesNotExist(ref table) => Error::not_found("table", table),
        redb::Error::TableExists(ref table) | redb::Error::TableAlreadyOpen(ref table, _) => {
            Error::already_exists("table", table)
        }
        redb::Error::ValueTooLarge(bytes) => Error::InvalidArgument(format!(
            "{context}: the value is {bytes} bytes, which exceeds what the database accepts",
        )),
        redb::Error::Corrupted(ref detail) => {
            Error::other(format!("{context}: the database is corrupted: {detail}"))
        }
        // Includes I/O failures, poisoned locks, a closed database and every
        // transaction-misuse variant. `redb::Error` is `#[non_exhaustive]`, so this arm is
        // also where any future variant lands: unclassified, and never silently retried.
        other => Error::wrap(context.to_owned(), StoreError(other)),
    }
}

/// As [`store_error`], for a failure raised while opening a database at a known path.
///
/// Exists for one variant: `DatabaseAlreadyOpen` is the only redb failure whose useful
/// message is the *path* rather than the operation, because what an operator needs to know
/// is which file is already claimed and therefore which other process to look for.
pub fn open_error(path: &Path, error: redb::DatabaseError) -> Error {
    match error {
        redb::DatabaseError::DatabaseAlreadyOpen => {
            Error::already_exists("database", path.display())
        }
        other => store_error("opening the database", other),
    }
}

/// Wraps an [`std::io::Error`] raised while touching the database's file or directory.
pub(crate) fn io_error(context: impl Into<String>, error: std::io::Error) -> Error {
    Error::wrap(context, error)
}

/// Adapts [`redb::Error`] into a [`BoxError`](aik_core::error::BoxError) source.
///
/// A newtype rather than boxing `redb::Error` directly, so that the chain a caller walks
/// keeps redb's own words as the *source* while the kernel's `{context}` stays the
/// top-level message — the shape every other subsystem's errors already have.
#[derive(Debug)]
struct StoreError(redb::Error);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    fn assert_kind(error: &Error, kind: ErrorKind) {
        assert_eq!(error.kind(), kind, "unexpected kind for `{error}`");
    }

    #[test]
    fn a_locked_database_names_the_file_and_is_a_conflict() {
        let error = open_error(
            Path::new("/var/lib/aik/aik.redb"),
            redb::DatabaseError::DatabaseAlreadyOpen,
        );
        assert_kind(&error, ErrorKind::Conflict);
        assert!(error.to_string().contains("/var/lib/aik/aik.redb"));
    }

    #[test]
    fn an_old_file_format_is_reported_as_a_configuration_problem() {
        let error = open_error(
            Path::new("/var/lib/aik/aik.redb"),
            redb::DatabaseError::UpgradeRequired(2),
        );
        assert_kind(&error, ErrorKind::Config);
        assert!(error.to_string().contains("v2"));
        assert!(error.to_string().contains("upgraded manually"));
    }

    #[test]
    fn a_missing_table_is_a_lookup_miss() {
        let error = store_error(
            "opening the meta table",
            redb::TableError::TableDoesNotExist("aik.meta".to_owned()),
        );
        assert_kind(&error, ErrorKind::NotFound);
        assert!(error.to_string().contains("aik.meta"));
    }

    #[test]
    fn a_table_of_the_wrong_shape_is_a_configuration_problem() {
        let error = store_error(
            "opening the meta table",
            redb::TableError::TableIsMultimap("aik.meta".to_owned()),
        );
        assert_kind(&error, ErrorKind::Config);
        assert!(error.to_string().contains("incompatible binary"));
    }

    #[test]
    fn an_oversized_value_blames_the_caller() {
        let error = store_error(
            "storing a record",
            redb::Error::ValueTooLarge(4 * 1024 * 1024 * 1024),
        );
        assert_kind(&error, ErrorKind::InvalidArgument);
        assert!(error.to_string().contains("storing a record"));
    }

    #[test]
    fn corruption_names_itself_and_is_never_a_retryable_kind() {
        let error = store_error(
            "reading the schema version",
            redb::Error::Corrupted("bad checksum".to_owned()),
        );
        assert_kind(&error, ErrorKind::Other);
        assert!(error.to_string().contains("corrupted"));
        assert!(error.to_string().contains("bad checksum"));
    }

    #[test]
    fn unclassified_failures_keep_the_context_and_the_underlying_cause() {
        let error = store_error(
            "committing the transaction",
            redb::Error::Io(std::io::Error::other("disk on fire")),
        );
        assert_kind(&error, ErrorKind::Other);
        assert_eq!(error.to_string(), "committing the transaction");
        let source = std::error::Error::source(&error).expect("the redb error is the source");
        assert!(source.to_string().contains("disk on fire"));
    }
}
