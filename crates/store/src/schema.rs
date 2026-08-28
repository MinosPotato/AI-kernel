//! The schema version stored in the database, and the migrations that raise it.
//!
//! # The rule this module enforces
//!
//! A database carries the version of the code that wrote it. Opening one is therefore a
//! three-way decision:
//!
//! * **older** — run the migrations between its version and this build's, then record the
//!   new version;
//! * **equal** — proceed;
//! * **newer** — refuse to open it.
//!
//! The last case is the one worth being explicit about. A newer database was written by a
//! build that knows tables, fields or invariants this one does not. Opening it read-write
//! would let an old binary write records the new binary cannot interpret, or drop fields it
//! does not know to preserve — silent, cumulative data loss that surfaces long after the
//! downgrade that caused it. Refusing is the only safe answer, so this fails closed. That
//! is also why a migration only ever moves forward: there is no down-migration, because a
//! correct one cannot be written for a schema whose semantics the running build predates.
//!
//! # What a migration is, and is not
//!
//! A migration transforms *data that already exists*. It is not how tables come into being:
//! redb creates a table on first
//! [`open_table`](redb::WriteTransaction::open_table), so a table a future version
//! introduces needs no migration to appear. A freshly created database is therefore stamped
//! with [`SCHEMA_VERSION`] directly and runs no migrations at all — there is nothing yet to
//! transform.

use aik_core::{Error, Result};
use redb::{Database, ReadableTable, TableDefinition, TableHandle, WriteTransaction};

use crate::error::store_error;

/// The schema version this build writes and understands.
///
/// # What a version means here
///
/// It records **which build wrote the file**, not what the file contains. Tables are created
/// lazily by whichever subsystem defines them (see
/// [the module documentation](self#what-a-migration-is-and-is-not)), so a kernel assembled
/// without `aik-memory` still stamps a file with the version below and simply never creates
/// `mem.*`. Reading a version as an inventory of tables would therefore be wrong; reading it
/// as a lower bound on the build that last had the file is exactly right, and is the only
/// thing this value is used for.
///
/// | Version | Refuses builds that predate |
/// |---|---|
/// | 1 | — (never released; the store's first published version was 2) |
/// | 2 | `context.sessions`, `context.records`, `context.record_ids`, owned by `aik-context` |
/// | 3 | `mem.records`, `mem.by_kind`, `mem.by_expiry`, owned by `aik-memory` |
/// | 4 | `mem.by_owner`, and the `owner` field every `mem.records` row now carries |
/// | 5 | `sched.jobs`, owned by `aik-scheduler` |
/// | 6 | `context.by_owner`, `context.by_updated`, owned by `aik-context` |
/// | 7 | `audit.records`, `audit.by_time`, `audit.by_principal`, `audit.by_correlation`, `audit.meta`, owned by `aik-audit` |
/// | 8 | `quota.usage`, owned by `aik-quota` |
///
/// A subsystem that adds tables raises this even though redb needs no migration to create
/// one. The version is what stops an older build from opening the file afterwards, and an
/// older build is exactly the one that does not know those tables exist — so it would not
/// preserve them through a compaction, a repair or a `clear` that walks the schema. That is
/// the whole of what a bump buys, and it is worth a bump on its own.
///
/// For the scheduler the argument is sharper than "space is not reclaimed": an older build
/// silently dropping `sched.jobs` would leave a system that looks healthy while the work it
/// was told to do at 3am simply never happens again. For the audit trail it is sharper still:
/// an older build that does not know `audit.*` exists is one that would carry a compaction or
/// a repair over the record of what this system was allowed to do — the one collection whose
/// value is precisely that nobody could quietly shorten it.
///
/// For the quota ledger it is sharper in a third way. `quota.usage` is the only table here
/// whose *absence* is permissive: a build that dropped it would not fail, it would report
/// every budget as untouched, and the deployment would go on looking healthy while spending
/// without a ceiling. A version bump is what stops such a build from opening the file.
pub const SCHEMA_VERSION: u32 = 8;

/// The table holding the store's own bookkeeping, keyed by a short ASCII name.
///
/// Namespaced under `aik.` so it cannot collide with a table a subsystem adds later.
pub(crate) const META: TableDefinition<'static, &str, u32> = TableDefinition::new("aik.meta");

/// The key [`SCHEMA_VERSION`] is stored under in [`META`].
pub(crate) const SCHEMA_VERSION_KEY: &str = "schema_version";

/// One forward step between two schema versions.
pub(crate) struct Migration {
    /// The version the database is at once [`Migration::apply`] has succeeded.
    pub to: u32,
    /// Transforms the data. Runs inside the caller's write transaction, never its own, so
    /// that a failure anywhere in the sequence rolls the whole upgrade back.
    pub apply: fn(&WriteTransaction) -> Result<()>,
}

impl std::fmt::Debug for Migration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Migration").field("to", &self.to).finish()
    }
}

/// Every migration this build knows, in ascending order of [`Migration::to`].
///
/// Still empty, which is a claim about released versions rather than about luck:
///
/// * **1** was never released; the store's first published version was 2.
/// * **2 → 3** added tables. redb creates a table on first `open_table`, so there was
///   nothing to transform (see [the module documentation](self#what-a-migration-is-and-is-not)).
/// * **3 → 4** added the `owner` field to `mem.records` and the `mem.by_owner` index, which
///   *is* a change of shape — but version 3 was never released either, so no database
///   outside a working tree can hold a `mem.records` row in the old shape. One that somehow
///   does fails loudly when the row will not decode, rather than being silently read as
///   though the field had a sensible default; there is no defensible default for "whose
///   memory is this".
/// * **4 → 5** added `sched.jobs`. A table, so again nothing to transform.
/// * **5 → 6** added `context.by_owner` and `context.by_updated`. Tables again — but this is
///   the first bump whose tables must also be *populated* from rows that already exist, and
///   it is still not a migration. Both are derived entirely from `context.sessions`, and
///   `aik-context` reconciles them against those headers every time it opens the database.
///   So the subsystem that owns the encoding does the backfill, in its own code, on a path
///   that also repairs drift — rather than this crate learning to decode a session header in
///   order to run the same loop once.
/// * **6 → 7** added the five `audit.*` tables. Tables again, so nothing to transform, and
///   nothing to backfill either: a trail records what happened while it was running, and
///   inventing records for the period before it existed would be the one thing an audit trail
///   must never do.
/// * **7 → 8** added `quota.usage`. A table, so nothing to transform; and nothing to backfill
///   either, for the opposite reason to the audit trail's. A ledger counts a window that is
///   open now, and every window that predates the ledger has closed — so a fresh table is not
///   an incomplete one, it is the correct one.
///
/// Adding a real migration means appending an entry here *and* raising [`SCHEMA_VERSION`] to
/// match its `to`; the invariant between the two is checked by [`migrate`] on every open.
///
/// # An open question, deliberately not answered yet
///
/// A migration that transforms a subsystem's rows has to know that subsystem's encoding,
/// which this crate deliberately does not — it owns `aik.meta` and nothing else. Every
/// migration so far has been a no-op, so the question of where such code lives (here, with
/// rows treated as opaque JSON; or registered by the subsystem that owns the table) has not
/// had to be answered. It should be answered the first time a *released* version needs its
/// data transformed, and not before, because the shape of that first real migration is what
/// should decide it.
pub(crate) const MIGRATIONS: &[Migration] = &[];

/// Brings a database up to `target`, or fails without modifying it.
///
/// `migrations` is a parameter rather than a direct reference to [`MIGRATIONS`] so that the
/// runner itself is testable: at [`SCHEMA_VERSION`] 1 the real list is empty, and a runner
/// that has never run a migration is not one worth trusting with data.
///
/// The whole upgrade — every migration, plus recording the new version — happens in a
/// single write transaction, so a crash part-way leaves the database at its original
/// version with its original contents.
pub(crate) fn migrate(db: &Database, target: u32, migrations: &[Migration]) -> Result<()> {
    check_ordering(target, migrations)?;

    let transaction = db
        .begin_write()
        .map_err(|error| store_error("beginning the schema-version transaction", error))?;

    let current = read_version(&transaction)?;
    match current {
        Some(version) if version == target => return Ok(()),
        Some(version) if version > target => {
            return Err(Error::config(
                "components.store.db.path",
                format!(
                    "the database is at schema version {version} but this build understands \
                     only version {target}; it was written by a newer build of aik and \
                     opening it here could silently destroy data",
                ),
            ));
        }
        Some(version) => {
            for migration in migrations.iter().filter(|m| m.to > version) {
                (migration.apply)(&transaction)?;
            }
        }
        None => {}
    }

    write_version(&transaction, target)?;
    transaction
        .commit()
        .map_err(|error| store_error("committing the schema version", error))
}

/// Reads the recorded version, distinguishing a fresh database from one written by
/// something that is not this store.
///
/// A database with tables in it but no version is not an old aik database — every version
/// of this store has written one — so it is something else entirely, and writing aik's
/// tables into it would corrupt whatever it actually is. That fails closed too.
fn read_version(transaction: &WriteTransaction) -> Result<Option<u32>> {
    let table = transaction
        .open_table(META)
        .map_err(|error| store_error("opening the meta table", error))?;
    let recorded = table
        .get(SCHEMA_VERSION_KEY)
        .map_err(|error| store_error("reading the schema version", error))?
        .map(|value| value.value());
    drop(table);

    if recorded.is_none() {
        let foreign: Vec<String> = transaction
            .list_tables()
            .map_err(|error| store_error("listing the database's tables", error))?
            .map(|handle| handle.name().to_owned())
            .filter(|name| name != META.name())
            .collect();
        if !foreign.is_empty() {
            return Err(Error::config(
                "components.store.db.path",
                format!(
                    "the database records no schema version but already holds tables \
                     ({}); it was not written by aik and will not be used",
                    foreign.join(", "),
                ),
            ));
        }
    }

    Ok(recorded)
}

/// Records `version` as the database's schema version.
fn write_version(transaction: &WriteTransaction, version: u32) -> Result<()> {
    let mut table = transaction
        .open_table(META)
        .map_err(|error| store_error("opening the meta table", error))?;
    table
        .insert(SCHEMA_VERSION_KEY, version)
        .map_err(|error| store_error("recording the schema version", error))?;
    Ok(())
}

/// Rejects a migration list that cannot correctly reach `target`.
///
/// Both failures are programming errors in this crate rather than anything an operator did,
/// but they are checked at runtime anyway: the cost is one pass over a handful of entries
/// once per open, and the alternative to catching them here is catching them after a
/// half-applied upgrade has already touched real data.
fn check_ordering(target: u32, migrations: &[Migration]) -> Result<()> {
    let mut previous = 0;
    for migration in migrations {
        if migration.to <= previous {
            return Err(Error::other(format!(
                "the migration list is out of order: version {} follows {previous}",
                migration.to,
            )));
        }
        previous = migration.to;
    }

    match migrations.last() {
        Some(last) if last.to != target => Err(Error::other(format!(
            "the last migration produces version {} but the target schema version is {target}",
            last.to,
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;
    use redb::{ReadableDatabase, ReadableTableMetadata};
    use std::path::Path;

    /// A table the test migrations append to, so that what ran — and in what order — is
    /// observable from the database itself rather than from a global the tests would have
    /// to serialise on.
    const TRACE: TableDefinition<'static, u32, u32> = TableDefinition::new("test.trace");

    fn record(transaction: &WriteTransaction, to: u32) -> Result<()> {
        let mut table = transaction.open_table(TRACE).unwrap();
        let position = u32::try_from(table.len().unwrap()).unwrap();
        table.insert(position, to).unwrap();
        Ok(())
    }

    fn to_two(transaction: &WriteTransaction) -> Result<()> {
        record(transaction, 2)
    }

    fn to_three(transaction: &WriteTransaction) -> Result<()> {
        record(transaction, 3)
    }

    fn refuses(_: &WriteTransaction) -> Result<()> {
        Err(Error::other("this migration always fails"))
    }

    fn database(path: &Path) -> Database {
        redb::Builder::new().create(path).unwrap()
    }

    fn set_version(db: &Database, version: u32) {
        let transaction = db.begin_write().unwrap();
        write_version(&transaction, version).unwrap();
        transaction.commit().unwrap();
    }

    fn version(db: &Database) -> Option<u32> {
        let transaction = db.begin_read().unwrap();
        let table = match transaction.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return None,
            Err(error) => panic!("{error}"),
        };
        table
            .get(SCHEMA_VERSION_KEY)
            .unwrap()
            .map(|value| value.value())
    }

    fn trace(db: &Database) -> Vec<u32> {
        let transaction = db.begin_read().unwrap();
        let table = match transaction.open_table(TRACE) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Vec::new(),
            Err(error) => panic!("{error}"),
        };
        table
            .iter()
            .unwrap()
            .map(|entry| entry.unwrap().1.value())
            .collect()
    }

    fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aik.redb");
        (directory, path)
    }

    #[test]
    fn a_fresh_database_is_stamped_with_the_target_version_and_runs_nothing() {
        let (_guard, path) = temp();
        let db = database(&path);

        migrate(
            &db,
            3,
            &[
                Migration {
                    to: 2,
                    apply: to_two,
                },
                Migration {
                    to: 3,
                    apply: to_three,
                },
            ],
        )
        .unwrap();

        assert_eq!(version(&db), Some(3));
        assert!(
            trace(&db).is_empty(),
            "a database with no data has nothing to migrate"
        );
    }

    #[test]
    fn a_database_already_at_the_target_is_left_alone() {
        let (_guard, path) = temp();
        let db = database(&path);
        set_version(&db, 3);

        migrate(
            &db,
            3,
            &[
                Migration {
                    to: 2,
                    apply: to_two,
                },
                Migration {
                    to: 3,
                    apply: to_three,
                },
            ],
        )
        .unwrap();

        assert_eq!(version(&db), Some(3));
        assert!(trace(&db).is_empty());
    }

    #[test]
    fn an_older_database_runs_only_the_migrations_above_its_version_in_order() {
        let (_guard, path) = temp();
        let db = database(&path);
        set_version(&db, 1);

        migrate(
            &db,
            3,
            &[
                Migration {
                    to: 2,
                    apply: to_two,
                },
                Migration {
                    to: 3,
                    apply: to_three,
                },
            ],
        )
        .unwrap();

        assert_eq!(version(&db), Some(3));
        assert_eq!(trace(&db), vec![2, 3], "migrations run in ascending order");
    }

    #[test]
    fn a_migration_already_applied_is_not_applied_again() {
        let (_guard, path) = temp();
        let db = database(&path);
        set_version(&db, 2);

        migrate(
            &db,
            3,
            &[
                Migration {
                    to: 2,
                    apply: to_two,
                },
                Migration {
                    to: 3,
                    apply: to_three,
                },
            ],
        )
        .unwrap();

        assert_eq!(version(&db), Some(3));
        assert_eq!(trace(&db), vec![3], "the 1 -> 2 step was already recorded");
    }

    #[test]
    fn a_newer_database_is_refused_and_left_exactly_as_it_was() {
        let (_guard, path) = temp();
        let db = database(&path);
        set_version(&db, 9);

        let error = migrate(
            &db,
            3,
            &[
                Migration {
                    to: 2,
                    apply: to_two,
                },
                Migration {
                    to: 3,
                    apply: to_three,
                },
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(error.to_string().contains("schema version 9"));
        assert!(error.to_string().contains("newer build"));
        assert_eq!(version(&db), Some(9), "the version was not rewritten");
        assert!(trace(&db).is_empty(), "no migration ran");
    }

    #[test]
    fn a_failing_migration_rolls_back_the_whole_upgrade() {
        let (_guard, path) = temp();
        let db = database(&path);
        set_version(&db, 1);

        let error = migrate(
            &db,
            3,
            &[
                Migration {
                    to: 2,
                    apply: to_two,
                },
                Migration {
                    to: 3,
                    apply: refuses,
                },
            ],
        )
        .unwrap_err();

        assert!(error.to_string().contains("always fails"));
        assert_eq!(version(&db), Some(1), "the version stayed where it was");
        assert!(
            trace(&db).is_empty(),
            "the successful first step was rolled back with the failed second"
        );
    }

    #[test]
    fn a_database_holding_tables_but_no_version_is_refused() {
        let (_guard, path) = temp();
        let db = database(&path);
        let transaction = db.begin_write().unwrap();
        record(&transaction, 1).unwrap();
        transaction.commit().unwrap();

        let error = migrate(&db, 1, &[]).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(error.to_string().contains("test.trace"));
        assert!(error.to_string().contains("not written by aik"));
        assert_eq!(version(&db), None, "nothing was stamped onto it");
    }

    #[test]
    fn the_meta_table_alone_still_counts_as_a_fresh_database() {
        let (_guard, path) = temp();
        let db = database(&path);
        // Opening the meta table without writing a version is what an interrupted first
        // open leaves behind; it must not be mistaken for a foreign database.
        let transaction = db.begin_write().unwrap();
        transaction.open_table(META).unwrap();
        transaction.commit().unwrap();

        migrate(&db, 1, &[]).unwrap();

        assert_eq!(version(&db), Some(1));
    }

    #[test]
    fn the_shipped_migrations_can_reach_the_shipped_version() {
        check_ordering(SCHEMA_VERSION, MIGRATIONS).expect("the shipped schema is self-consistent");
    }

    #[test]
    fn an_out_of_order_migration_list_is_refused() {
        let migrations = [
            Migration {
                to: 3,
                apply: |_| Ok(()),
            },
            Migration {
                to: 2,
                apply: |_| Ok(()),
            },
        ];
        let error = check_ordering(3, &migrations).unwrap_err();
        assert!(error.to_string().contains("out of order"));
    }

    #[test]
    fn a_migration_list_that_stops_short_of_the_target_is_refused() {
        let migrations = [Migration {
            to: 2,
            apply: |_| Ok(()),
        }];
        let error = check_ordering(3, &migrations).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(error.to_string().contains("produces version 2"));
    }
}
