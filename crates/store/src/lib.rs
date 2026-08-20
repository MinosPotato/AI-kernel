//! The embedded database the kernel's durable subsystems share.
//!
//! This crate opens one [`redb`] database, secures its file, brings its schema to the
//! version this build understands, and registers it as a kernel service. It implements no
//! domain contract: there is no `ContextStore` and no `MemoryStore` here, and there never
//! will be. Those live in their own crates, behind their own traits, and share only the file
//! underneath.
//!
//! # Why one database rather than one per subsystem
//!
//! redb takes an exclusive lock per file, so a shared database is a shared *handle* — it
//! cannot be reopened per subsystem even if that were desirable. Making that constraint
//! explicit buys three things: one schema version rather than several that can disagree,
//! one file to back up or delete, and the option of a write that spans two subsystems'
//! tables in a single transaction. What it deliberately does not buy is a shared *schema*.
//! Tables are namespaced and owned by the subsystem that defines them; this crate owns only
//! `aik.meta`.
//!
//! # Why redb is not wrapped
//!
//! [`Db::database`] hands out the [`redb::Database`]. An abstraction over it would have to
//! reinvent typed tables, ranges and transactions before it was useful for anything, and
//! callers would reach past it the moment it fell short. What is centralised here is the
//! part that genuinely must be consistent across subsystems and is easy to get wrong
//! separately: how the file is created, how failures are classified
//! ([`store_error`]), and what version the schema is at ([`schema`]).
//!
//! redb is re-exported so that subsystems depending on this crate cannot end up compiling
//! against a different version of it than the one that opened the database.
//!
//! # What it guarantees
//!
//! * **The file is the owner's alone.** Created at `0600` inside a directory created at
//!   `0700`; an existing file that other accounts can read is refused rather than used. See
//!   [`Db::open`].
//! * **A newer database is never opened.** A file written by a later build of aik is
//!   refused, because an older binary writing to it would silently lose the fields it does
//!   not know about. See [`schema`].
//! * **An upgrade is all-or-nothing.** Every migration and the version stamp share one
//!   write transaction.
//! * **A foreign database is never adopted.** A file with tables but no schema version was
//!   not written by aik, and is refused rather than written into.
//!
//! # Getting one
//!
//! ```no_run
//! use aik_core::prelude::*;
//! use aik_store::StoreComponent;
//!
//! # fn build() -> Result<Kernel> {
//! Kernel::builder().component(StoreComponent::new()).build()
//! # }
//! ```
//!
//! Or, without a kernel:
//!
//! ```
//! use aik_store::{Db, SCHEMA_VERSION};
//!
//! # fn main() -> aik_core::Result<()> {
//! # let directory = tempfile::tempdir().unwrap();
//! let db = Db::open(directory.path().join("aik.redb"))?;
//! assert_eq!(db.schema_version()?, SCHEMA_VERSION);
//! # Ok(())
//! # }
//! ```

mod component;
mod db;
mod error;
pub mod schema;
mod settings;

pub use component::{DEFAULT_COMPONENT_ID, StoreComponent};
pub use db::{DATABASE_DIRECTORY_MODE, DATABASE_FILE_MODE, Db};
pub use error::{open_error, store_error};
pub use schema::SCHEMA_VERSION;
pub use settings::{DEFAULT_DIRECTORY_NAME, DEFAULT_FILE_NAME, StoreSettings, default_path};

/// The database engine, re-exported so subsystems share exactly one version of it.
pub use redb;
