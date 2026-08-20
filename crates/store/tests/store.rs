//! End-to-end behaviour of the store: opening a database, securing its file, and wiring it
//! into a kernel.
//!
//! Schema versioning and the migration runner are exercised as unit tests in
//! `src/schema.rs` instead, because writing a database *at another version* — which is what
//! the upgrade and refusal cases require — means writing the meta table directly, and that
//! is deliberately not public API.

use std::path::Path;
use std::sync::Arc;

use aik_core::prelude::*;
use aik_core::{Config, ErrorKind};
use aik_store::{DEFAULT_COMPONENT_ID, Db, SCHEMA_VERSION, StoreComponent, redb};
use serde_json::json;

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A table the tests write through the public `Db::database` handle, to prove the database
/// a caller gets back is a working one rather than merely an opened file.
const PROBE: redb::TableDefinition<'static, &str, u32> = redb::TableDefinition::new("test.probe");

fn put(db: &Db, key: &str, value: u32) {
    let transaction = db.database().begin_write().unwrap();
    {
        let mut table = transaction.open_table(PROBE).unwrap();
        table.insert(key, value).unwrap();
    }
    transaction.commit().unwrap();
}

fn get(db: &Db, key: &str) -> Option<u32> {
    use redb::ReadableDatabase;
    let transaction = db.database().begin_read().unwrap();
    let table = transaction.open_table(PROBE).unwrap();
    table.get(key).unwrap().map(|value| value.value())
}

/// The store's configuration section, nested to match its component id.
///
/// `components.<id>` is a dotted config path, so the dot in `store.db` is nesting rather
/// than part of a key — the same shape `aik-ollama` needs for `model.ollama`. Getting this
/// wrong does not fail loudly: the section simply reads as absent and the component falls
/// back to its defaults, which for this one means the operator's real data directory.
fn config_for(path: &Path) -> Config {
    Config::builder()
        .layer(json!({
            "components": { "store": { "db": { "path": path } } }
        }))
        .build()
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_fresh_database_is_created_at_the_current_schema_version() {
    let directory = temp();
    let path = directory.path().join("aik.redb");

    let db = Db::open(&path).unwrap();

    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(db.path(), path);
    assert!(path.exists());
}

#[test]
fn missing_parent_directories_are_created() {
    let directory = temp();
    let path = directory
        .path()
        .join("nested")
        .join("deeper")
        .join("aik.redb");

    let db = Db::open(&path).unwrap();

    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    assert!(path.parent().unwrap().is_dir());
}

#[test]
fn a_database_survives_being_closed_and_reopened() {
    let directory = temp();
    let path = directory.path().join("aik.redb");

    let db = Db::open(&path).unwrap();
    put(&db, "answer", 42);
    drop(db);

    let reopened = Db::open(&path).unwrap();
    assert_eq!(get(&reopened, "answer"), Some(42));
    assert_eq!(
        reopened.schema_version().unwrap(),
        SCHEMA_VERSION,
        "reopening does not re-stamp or re-migrate"
    );
}

#[test]
fn a_second_handle_on_the_same_file_is_refused_as_a_conflict() {
    let directory = temp();
    let path = directory.path().join("aik.redb");

    let _first = Db::open(&path).unwrap();
    let error = Db::open(&path).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert!(
        error.to_string().contains(&path.display().to_string()),
        "the message names the contended file, got `{error}`"
    );
}

#[cfg(unix)]
#[test]
fn the_database_file_is_readable_only_by_its_owner() {
    let directory = temp();
    let path = directory.path().join("aik.redb");

    let _db = Db::open(&path).unwrap();

    assert_eq!(
        mode_of(&path),
        0o600,
        "a file holding conversation transcripts must not be readable by other accounts"
    );
}

#[cfg(unix)]
#[test]
fn a_directory_the_store_creates_is_private() {
    let directory = temp();
    let parent = directory.path().join("created-by-the-store");
    let path = parent.join("aik.redb");

    let _db = Db::open(&path).unwrap();

    assert_eq!(mode_of(&parent), 0o700);
}

#[cfg(unix)]
#[test]
fn an_existing_file_other_accounts_can_read_is_refused_rather_than_used() {
    use std::os::unix::fs::PermissionsExt;

    let directory = temp();
    let path = directory.path().join("aik.redb");
    drop(Db::open(&path).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let error = Db::open(&path).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(error.to_string().contains("0644"));
    assert!(error.to_string().contains("chmod 600"));
    assert_eq!(
        mode_of(&path),
        0o644,
        "refusing does not quietly change the operator's file"
    );
}

#[cfg(unix)]
#[test]
fn a_mode_that_exposes_nothing_is_not_refused_by_the_permission_check() {
    use std::os::unix::fs::PermissionsExt;

    let directory = temp();
    let path = directory.path().join("aik.redb");
    drop(Db::open(&path).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();

    // Read-only defeats redb rather than this crate's check, so assert on which failure it
    // is: anything but the permission refusal means the check let it through.
    let error = Db::open(&path).unwrap_err();
    assert!(
        !error.to_string().contains("chmod 600"),
        "0400 exposes nothing to other accounts and must not be refused as if it did"
    );
}

#[tokio::test]
async fn the_component_opens_the_configured_database_and_registers_it() {
    let directory = temp();
    let path = directory.path().join("configured.redb");

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let db = kernel.context().service::<Db>().unwrap();
    assert_eq!(db.path(), path);
    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_component_can_declare_a_dependency_on_the_store_and_resolve_it() {
    struct Dependent {
        seen: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    }

    #[async_trait]
    impl Component for Dependent {
        fn descriptor(&self) -> ComponentDescriptor {
            ComponentDescriptor::new("test.dependent").requires(DEFAULT_COMPONENT_ID)
        }

        async fn init(&self, ctx: &ComponentContext) -> Result<()> {
            let db = ctx.service::<Db>()?;
            *self.seen.lock().unwrap() = Some(db.path().to_path_buf());
            Ok(())
        }
    }

    let directory = temp();
    let path = directory.path().join("shared.redb");
    let seen = Arc::new(std::sync::Mutex::new(None));

    let kernel = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .component(Dependent { seen: seen.clone() })
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    assert_eq!(
        seen.lock().unwrap().clone(),
        Some(path),
        "the store is initialised before anything that requires it"
    );

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_database_that_cannot_be_opened_fails_the_kernel_during_startup() {
    let directory = temp();
    // A path whose parent is a regular file cannot have a directory created at it.
    let blocker = directory.path().join("not-a-directory");
    std::fs::write(&blocker, b"").unwrap();
    let path = blocker.join("aik.redb");

    let error = Kernel::builder()
        .config(config_for(&path))
        .component(StoreComponent::new())
        .build()
        .unwrap()
        .start()
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Lifecycle);
    assert!(
        error.to_string().contains(DEFAULT_COMPONENT_ID),
        "the failure is attributed to the store component, got `{error}`"
    );
}

#[tokio::test]
async fn an_unconfigured_store_with_no_data_directory_refuses_to_guess_one() {
    // `resolve_path` reads the real environment, so this asserts on the resolver's own
    // contract rather than on this machine's: with no path configured it must consult the
    // environment, and the unit tests in `settings` cover what it does with each shape of
    // it. Here the point is only that an absent `path` is not silently turned into a file
    // in the working directory.
    let settings = aik_store::StoreSettings::default();
    let resolved = settings.resolve_path_from(Vec::<(String, String)>::new());

    let error = resolved.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(error.to_string().contains("set the path explicitly"));
}
