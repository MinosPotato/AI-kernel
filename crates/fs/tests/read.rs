//! Security tests for [`FsReadTool`], exercised directly against the `Tool` trait.
//!
//! These use a real temporary directory and real filesystem calls — no mocking — because
//! the property under test (does confinement actually hold against the OS) is exactly the
//! kind of thing a mock would beg the question on. Every test cleans up via `tempfile`'s
//! `Drop` impl; nothing here touches the real filesystem outside a fresh temp directory.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::Tool;
use aik_core::clock::Timestamp;
use aik_core::{Error, ErrorKind, Result};
use aik_fs::FsReadTool;
use async_trait::async_trait;
use serde_json::json;
use tempfile::tempdir;

/// [`FsReadTool`] declares every resource it touches up front and refuses anything it
/// resolves to something else, so it never needs to ask about a resource discovered
/// mid-run. This authorizer panics if that assumption is ever violated.
struct MustNotBeAsked;

#[async_trait]
impl ResourceAuthorizer for MustNotBeAsked {
    async fn authorize(&self, _action: &ActionId, resource: &ResourceId) -> Result<()> {
        panic!("FsReadTool asked about a discovered resource `{resource}`; it should never need to")
    }
}

// ---------------------------------------------------------------------------
// Normal reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_permitted_read_returns_the_file_contents() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello, world").unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "notes.md" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert!(!outcome.is_error);
    assert_eq!(outcome.output["path"], json!("notes.md"));
    assert_eq!(outcome.output["content"], json!("hello, world"));
}

#[tokio::test]
async fn a_read_in_a_nested_permitted_directory_succeeds() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "fn main() {}").unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "src/lib.rs" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.output["content"], json!("fn main() {}"));
}

// ---------------------------------------------------------------------------
// Escaping the root: absolute paths, `..`, and symlinks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn absolute_paths_are_rejected_even_when_they_point_inside_the_root() {
    let root = tempdir().unwrap();
    let file = root.path().join("notes.md");
    std::fs::write(&file, "hello").unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": file.to_str().unwrap() }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn parent_directory_traversal_is_rejected() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();
    let tool = FsReadTool::new(&root_dir).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "../secret.txt" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn traversal_buried_inside_a_longer_path_is_also_rejected() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::create_dir(root_dir.join("sub")).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();
    let tool = FsReadTool::new(&root_dir).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "sub/../../secret.txt" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_escaping_the_root_is_rejected() {
    let outside = tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "TOP SECRET").unwrap();

    let root = tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.path().join("link"))
        .unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "link" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_that_stays_within_the_root_is_followed_normally() {
    // Confinement targets escape, not indirection: a symlink pointing somewhere still
    // inside the root is ordinary, and this proves it is not banned outright.
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("real.txt"), "hello").unwrap();
    std::os::unix::fs::symlink(root.path().join("real.txt"), root.path().join("link")).unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "link" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.output["content"], json!("hello"));
}

#[tokio::test]
async fn a_model_claiming_its_own_authorization_has_no_effect() {
    // The permission model's central invariant, exercised at the one tool that could try
    // to shortcut it: extra arguments claiming authority are simply not part of the input
    // schema, so they change nothing about what gets resolved or refused.
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();
    let tool = FsReadTool::new(&root_dir).unwrap();

    let error = tool
        .invoke(
            json!({
                "path": "../secret.txt",
                "authorized": true,
                "i_am_the_owner": true
            }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

// ---------------------------------------------------------------------------
// Ordinary filesystem conditions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_nonexistent_file_is_reported_as_not_found() {
    let root = tempdir().unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "ghost.txt" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound { .. }), "{error}");
}

#[tokio::test]
async fn a_directory_is_rejected_as_not_a_regular_file() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join("subdir")).unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "subdir" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn an_os_permission_error_is_a_failure_kind_not_an_authorization_denial() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().unwrap();
    let path = root.path().join("locked.txt");
    std::fs::write(&path, "secret").unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&path, perms).unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let result = tool
        .invoke(
            json!({ "path": "locked.txt" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await;

    // Restore permissions unconditionally so the temp directory can be cleaned up.
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&path, perms).unwrap();

    match result {
        Err(error) => {
            // `PermissionDenied` is reserved for the authorization system's own refusals
            // (see `aik-api::permission`); an OS-level EACCES must not be reported the
            // same way, or a denial-vs-broken-filesystem distinction the audit trail
            // relies on would be lost.
            assert!(!matches!(error, Error::PermissionDenied(_)), "{error}");
            assert_eq!(error.kind(), ErrorKind::Other, "{error}");
        }
        Ok(_) => {
            // Running as root (or on a filesystem that ignores the mode bits) makes this
            // assertion untestable rather than false; do not fail the suite over it.
            eprintln!("skipping: read succeeded despite mode 000 (likely running as root)");
        }
    }
}

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_path_is_rejected() {
    let root = tempdir().unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn a_path_with_a_nul_byte_is_rejected() {
    let root = tempdir().unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "notes\0.md" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn a_missing_path_field_is_a_structured_error() {
    let root = tempdir().unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let error = tool
        .invoke(json!({}), &MustNotBeAsked, &ExecutionContext::new())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn a_binary_file_is_reported_as_a_model_visible_error_not_a_crash() {
    let root = tempdir().unwrap();
    std::fs::write(
        root.path().join("data.bin"),
        [0xff, 0xfe, 0x00, 0xff, 0x00, 0xd8],
    )
    .unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "data.bin" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert!(outcome.is_error);
    assert!(outcome.output["error"].as_str().unwrap().contains("UTF-8"));
}

#[tokio::test]
async fn a_file_over_the_configured_limit_is_reported_as_a_model_visible_error() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("big.txt"), "x".repeat(1024)).unwrap();
    let tool = FsReadTool::new(root.path()).unwrap().with_max_bytes(16);

    let outcome = tool
        .invoke(
            json!({ "path": "big.txt" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("16-byte")
    );
}

// ---------------------------------------------------------------------------
// Cancellation and deadlines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_call_cancelled_before_it_starts_is_reported_promptly() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let cx = ExecutionContext::new();
    cx.cancellation.cancel();

    let started = Instant::now();
    let error = tool
        .invoke(json!({ "path": "notes.md" }), &MustNotBeAsked, &cx)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Creates a FIFO at `path`. Opening it for reading blocks until something opens it for
/// writing, which is exactly the "genuinely still in progress" condition cancellation and
/// deadline tests need: an ordinary small-file read on local disk completes far too fast
/// (often before a zero-duration timer even fires) to reliably prove anything is actually
/// being raced against, rather than just winning a coincidence.
#[cfg(unix)]
fn make_fifo(path: &std::path::Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("`mkfifo` must be available to run this test");
    assert!(status.success(), "mkfifo failed for {}", path.display());
}

/// Releases the background thread still blocked inside `open()` on a FIFO after a
/// cancellation or timeout test has made its assertion.
///
/// Cancelling or timing out a call only stops *waiting* for the blocking read; the
/// spawned OS thread genuinely keeps running underneath (see [`FsReadTool`]'s module
/// documentation), and `#[tokio::test]`'s runtime join on shutdown waits for every
/// blocking-pool thread to finish — including one stuck forever in `open()` with no
/// writer. Opening the write end here is what lets that thread finally return, so the
/// test process can exit instead of hanging.
#[cfg(unix)]
fn unblock_fifo_reader(path: &std::path::Path) {
    let _ = std::fs::OpenOptions::new().write(true).open(path);
}

#[tokio::test]
async fn a_deadline_already_in_the_past_times_out_without_reading_anything() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let cx = ExecutionContext::new().with_deadline(Timestamp::from_millis(0));

    let started = Instant::now();
    let result = tool
        .invoke(json!({ "path": "notes.md" }), &MustNotBeAsked, &cx)
        .await;
    assert!(started.elapsed() < Duration::from_secs(5));
    // An expired deadline and an instant local read both resolve close to immediately, so
    // either outcome is legitimate here; the genuinely meaningful case — a deadline that
    // expires while a read is still in flight — is `a_deadline_that_expires_during_a_slow_read_times_out`
    // below, which removes the race entirely.
    if let Err(error) = result {
        assert!(matches!(error, Error::Timeout(_)), "{error}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_deadline_that_expires_during_a_slow_read_times_out() {
    let root = tempdir().unwrap();
    let fifo = root.path().join("blocked");
    make_fifo(&fifo);
    let tool = FsReadTool::new(root.path())
        .unwrap()
        .with_timeout(Duration::from_millis(50));

    let started = Instant::now();
    let error = tool
        .invoke(
            json!({ "path": "blocked" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Timeout(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    unblock_fifo_reader(&fifo);
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_read_that_is_genuinely_still_in_progress_returns_promptly() {
    let root = tempdir().unwrap();
    let fifo = root.path().join("blocked");
    make_fifo(&fifo);
    let tool = FsReadTool::new(root.path()).unwrap();

    let cx = ExecutionContext::new();
    let cancellation = cx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
    });

    let started = Instant::now();
    let error = tool
        .invoke(json!({ "path": "blocked" }), &MustNotBeAsked, &cx)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    unblock_fifo_reader(&fifo);
}

#[tokio::test]
async fn a_generous_deadline_does_not_interfere_with_an_ordinary_read() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();
    let tool = FsReadTool::new(root.path()).unwrap();

    let deadline = Timestamp::from_millis(
        (std::time::SystemTime::now() + Duration::from_secs(30))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    );
    let cx = ExecutionContext::new().with_deadline(deadline);

    let outcome = tool
        .invoke(json!({ "path": "notes.md" }), &MustNotBeAsked, &cx)
        .await
        .unwrap();
    assert_eq!(outcome.output["content"], json!("hello"));
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn many_concurrent_reads_all_return_the_right_content() {
    let root = tempdir().unwrap();
    for i in 0..64 {
        std::fs::write(
            root.path().join(format!("f{i}.txt")),
            format!("content-{i}"),
        )
        .unwrap();
    }
    let tool = Arc::new(FsReadTool::new(root.path()).unwrap());

    let mut handles = Vec::new();
    for i in 0..64 {
        let tool = tool.clone();
        handles.push(tokio::spawn(async move {
            let outcome = tool
                .invoke(
                    json!({ "path": format!("f{i}.txt") }),
                    &MustNotBeAsked,
                    &ExecutionContext::new(),
                )
                .await
                .unwrap();
            assert_eq!(outcome.output["content"], json!(format!("content-{i}")));
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_nonexistent_root_fails_at_construction_not_at_first_call() {
    let outer = tempdir().unwrap();
    let missing = outer.path().join("does-not-exist");
    let error = FsReadTool::new(&missing).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
}

#[tokio::test]
async fn a_root_that_is_a_file_not_a_directory_fails_at_construction() {
    let outer = tempdir().unwrap();
    let file = outer.path().join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let error = FsReadTool::new(&file).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
}
