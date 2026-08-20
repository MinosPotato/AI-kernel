//! [`Db`]: the opened database every durable subsystem shares.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use aik_core::{Error, Result};
use redb::{Database, ReadableDatabase};

use crate::error::{io_error, open_error, store_error};
use crate::schema::{META, MIGRATIONS, SCHEMA_VERSION, SCHEMA_VERSION_KEY, migrate};

/// The mode a newly created database file is given.
///
/// The database holds full conversation transcripts — the very content `ContextAssembled`
/// refuses to put in an event, because shipping it to anything that aggregates logs would be
/// a larger disclosure than anything it could report. Persisting it at a mode any other
/// account can read would undo that decision silently, so the owner is the only principal
/// that gets access.
pub const DATABASE_FILE_MODE: u32 = 0o600;

/// The mode the directory holding the database is created with.
///
/// The file's own mode is what protects its contents; this protects the fact of its
/// existence, and anything a subsystem later writes alongside it.
pub const DATABASE_DIRECTORY_MODE: u32 = 0o700;

/// An open database, at a known schema version.
///
/// # What holding one means
///
/// redb takes an exclusive lock on the file, so exactly one `Db` exists per database per
/// machine, and it is shared rather than reopened: [`StoreComponent`](crate::StoreComponent)
/// registers it in the kernel registry and durable subsystems resolve it from there. That
/// is not an inconvenience to work around — it is what makes a write that spans two
/// subsystems' tables a single transaction.
///
/// # Concurrency
///
/// redb is synchronous, and its API is used as such here. A write transaction is exclusive
/// and read transactions are MVCC snapshots that do not block it. Callers doing this work
/// from an async context are responsible for moving it onto a blocking thread — this type
/// deliberately does not wrap every operation in `tokio::task::spawn_blocking`, because the
/// useful unit to move is a whole transaction, which only the caller can delimit.
pub struct Db {
    database: Database,
    path: PathBuf,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("path", &self.path).finish()
    }
}

impl Db {
    /// Opens the database at `path`, creating it if it does not exist, and brings its
    /// schema up to [`SCHEMA_VERSION`].
    ///
    /// The parent directory is created if missing. Returns an error, having modified
    /// nothing, if the database is at a newer schema version than this build understands —
    /// see [`schema`](crate::schema).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            create_directory(parent)?;
        }
        let file = open_file(&path)?;

        let database = redb::Builder::new()
            .create_file(file)
            .map_err(|error| open_error(&path, error))?;
        migrate(&database, SCHEMA_VERSION, MIGRATIONS)?;

        Ok(Self { database, path })
    }

    /// Where this database lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The underlying redb database.
    ///
    /// Durable subsystems define their own tables and run their own transactions against
    /// this. It is exposed rather than wrapped deliberately: an abstraction over redb would
    /// have to reinvent typed tables, ranges and transactions to be useful, and would leave
    /// every caller reaching past it the moment it fell short.
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// The schema version recorded in the database.
    ///
    /// Always [`SCHEMA_VERSION`] for a database opened by this build, since [`Db::open`]
    /// either migrates it there or refuses. Exposed for diagnostics and for tests that need
    /// to observe an upgrade actually happened.
    pub fn schema_version(&self) -> Result<u32> {
        let transaction = self
            .database
            .begin_read()
            .map_err(|error| store_error("reading the schema version", error))?;
        let table = transaction
            .open_table(META)
            .map_err(|error| store_error("opening the meta table", error))?;
        let version = table
            .get(SCHEMA_VERSION_KEY)
            .map_err(|error| store_error("reading the schema version", error))?
            .map(|value| value.value())
            .ok_or_else(|| Error::other("the database records no schema version"))?;
        Ok(version)
    }
}

/// Creates the directory holding the database, at [`DATABASE_DIRECTORY_MODE`].
///
/// The mode applies only to directories this call creates. An existing directory keeps its
/// own permissions: the parents of a data directory are routinely group-readable
/// (`~/.local/share` is conventionally `0755`), and tightening them would reach well
/// outside what this crate owns. The database file's own mode is what protects its
/// contents.
fn create_directory(parent: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DATABASE_DIRECTORY_MODE);
    }
    builder
        .create(parent)
        .map_err(|error| io_error(format!("creating {}", parent.display()), error))
}

/// Opens the database file, creating it at [`DATABASE_FILE_MODE`] if it does not exist.
///
/// An existing file that any other account can read or write is refused rather than
/// silently tightened or silently used: a mode this crate did not choose is either a
/// deliberate operator decision — in which case it should be an explicit one, not
/// overridden here — or an accident that has been exposing transcripts, in which case
/// carrying on as if nothing were wrong is the one thing not to do.
#[cfg(unix)]
fn open_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Ok(metadata) = std::fs::metadata(path) {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(Error::config(
                "components.store.db.path",
                format!(
                    "{} is mode {mode:04o}, which lets other accounts read the conversation \
                     transcripts it holds; run `chmod 600 {}` and start again",
                    path.display(),
                    path.display(),
                ),
            ));
        }
    }

    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(DATABASE_FILE_MODE)
        .open(path)
        .map_err(|error| io_error(format!("opening {}", path.display()), error))
}

/// The portable fallback: this platform has no Unix file modes, so the file is created with
/// whatever the platform's defaults are and the permission check above is not performed.
#[cfg(not(unix))]
fn open_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| io_error(format!("opening {}", path.display()), error))
}
