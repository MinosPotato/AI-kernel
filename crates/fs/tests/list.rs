//! Security tests for [`FsListTool`], exercised directly against the `Tool` trait.
//!
//! Unlike [`FsReadTool`](aik_fs::FsReadTool) and [`FsWriteTool`](aik_fs::FsWriteTool),
//! `FsListTool` genuinely asks its [`ResourceAuthorizer`] about resources discovered
//! mid-run — every entry it finds — so these tests exercise real authorizers, not just a
//! `MustNotBeAsked` stand-in. Policy-level "narrowing" (a real `PolicyEngine` denying some
//! entries but not others through the full `ToolRegistry`) is covered separately in
//! `end_to_end.rs`; these tests are about what the tool itself does with whatever answer it
//! is given.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::Tool;
use aik_core::clock::Timestamp;
use aik_core::{Error, ErrorKind, Result};
use aik_fs::FsListTool;
use async_trait::async_trait;
use serde_json::json;
use tempfile::tempdir;

/// Allows every discovered resource, and records what it was asked about, in order.
#[derive(Default)]
struct Recording {
    asked: Mutex<Vec<(ActionId, ResourceId)>>,
}

#[async_trait]
impl ResourceAuthorizer for Recording {
    async fn authorize(&self, action: &ActionId, resource: &ResourceId) -> Result<()> {
        self.asked
            .lock()
            .unwrap()
            .push((action.clone(), resource.clone()));
        Ok(())
    }
}

/// Denies any resource whose path ends with one of the given suffixes; allows everything
/// else. Simulates a policy that narrows what a directory listing reveals.
struct DenySuffixes(&'static [&'static str]);

#[async_trait]
impl ResourceAuthorizer for DenySuffixes {
    async fn authorize(&self, _action: &ActionId, resource: &ResourceId) -> Result<()> {
        if self
            .0
            .iter()
            .any(|suffix| resource.as_str().ends_with(suffix))
        {
            Err(Error::PermissionDenied(format!("denied: {resource}")))
        } else {
            Ok(())
        }
    }
}

/// Never resolves, so cancellation or a deadline is the only thing that can end the call —
/// proving the per-entry authorization loop actually honours both rather than only checking
/// them before the scan.
struct HangForever;

#[async_trait]
impl ResourceAuthorizer for HangForever {
    async fn authorize(&self, _action: &ActionId, _resource: &ResourceId) -> Result<()> {
        std::future::pending::<()>().await;
        unreachable!("this authorizer never resolves")
    }
}

fn entry_names(output: &serde_json::Value) -> BTreeSet<String> {
    output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Normal listings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_directory_lists_no_entries_and_asks_nothing() {
    let root = tempdir().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();
    let authorizer = Recording::default();

    let outcome = tool
        .invoke(json!({ "path": "" }), &authorizer, &ExecutionContext::new())
        .await
        .unwrap();

    assert!(!outcome.is_error);
    assert_eq!(outcome.output["path"], json!(""));
    assert_eq!(entry_names(&outcome.output), BTreeSet::new());
    assert!(authorizer.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn omitting_path_lists_the_root_itself() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hi").unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(json!({}), &Recording::default(), &ExecutionContext::new())
        .await
        .unwrap();

    assert_eq!(
        entry_names(&outcome.output),
        BTreeSet::from(["notes.md".to_owned()])
    );
}

#[tokio::test]
async fn files_directories_and_symlinks_are_classified_correctly() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "hello").unwrap();
    std::fs::create_dir(root.path().join("subdir")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("file.txt"), root.path().join("link")).unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    let entries = outcome.output["entries"].as_array().unwrap();
    let kind_of = |name: &str| {
        entries
            .iter()
            .find(|e| e["name"] == json!(name))
            .unwrap_or_else(|| panic!("entry `{name}` missing"))["kind"]
            .clone()
    };
    assert_eq!(kind_of("file.txt"), json!("file"));
    assert_eq!(kind_of("subdir"), json!("directory"));
    #[cfg(unix)]
    assert_eq!(kind_of("link"), json!("symlink"));

    let file_entry = entries
        .iter()
        .find(|e| e["name"] == json!("file.txt"))
        .unwrap();
    assert_eq!(file_entry["size"], json!(5));
    let dir_entry = entries
        .iter()
        .find(|e| e["name"] == json!("subdir"))
        .unwrap();
    assert_eq!(dir_entry["size"], json!(null));
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinks_size_is_never_reported() {
    // Reporting a symlink's size would mean reading it (its target's length), which this
    // tool never does — see `FsListTool`'s documentation on why symlinks are never followed.
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("target-with-a-long-name.txt"), "x").unwrap();
    std::os::unix::fs::symlink(
        root.path().join("target-with-a-long-name.txt"),
        root.path().join("link"),
    )
    .unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    let link = outcome.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == json!("link"))
        .unwrap();
    assert_eq!(link["size"], json!(null));
}

#[cfg(unix)]
#[tokio::test]
async fn special_files_are_reported_as_other_and_never_opened() {
    let root = tempdir().unwrap();
    let fifo = root.path().join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());
    let tool = FsListTool::new(root.path()).unwrap();

    let started = Instant::now();
    let outcome = tool
        .invoke(
            json!({ "path": "" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    // Opening the FIFO for reading would block forever with no writer; listing must never
    // do that, so this proves it by finishing promptly.
    assert!(started.elapsed() < Duration::from_secs(5));

    let entries = outcome.output["entries"].as_array().unwrap();
    assert_eq!(entries[0]["name"], json!("pipe"));
    assert_eq!(entries[0]["kind"], json!("other"));
    assert_eq!(entries[0]["size"], json!(null));
}

#[tokio::test]
async fn entries_are_sorted_by_name() {
    let root = tempdir().unwrap();
    for name in ["banana", "apple", "cherry"] {
        std::fs::write(root.path().join(name), "").unwrap();
    }
    let tool = FsListTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    let names: Vec<String> = outcome.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, vec!["apple", "banana", "cherry"]);
}

#[tokio::test]
async fn a_nested_permitted_directory_lists_its_own_entries_not_the_roots() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("top.txt"), "").unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "src" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        entry_names(&outcome.output),
        BTreeSet::from(["lib.rs".to_owned()])
    );
}

// ---------------------------------------------------------------------------
// Discovered-resource authorization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_entry_is_asked_about_individually_with_the_lists_own_action() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "").unwrap();
    std::fs::write(root.path().join("b.txt"), "").unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();
    let authorizer = Recording::default();

    tool.invoke(json!({ "path": "" }), &authorizer, &ExecutionContext::new())
        .await
        .unwrap();

    let asked = authorizer.asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    let resources: BTreeSet<String> = asked.iter().map(|(_, r)| r.as_str().to_owned()).collect();
    assert_eq!(
        resources,
        BTreeSet::from([
            canonical.join("a.txt").to_string_lossy().into_owned(),
            canonical.join("b.txt").to_string_lossy().into_owned(),
        ])
    );
    assert!(
        asked
            .iter()
            .all(|(action, _)| action.as_str() == "filesystem.list"),
        "{asked:?}"
    );
}

#[tokio::test]
async fn an_entry_the_authorizer_denies_is_left_out_but_the_call_still_succeeds() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("visible.txt"), "").unwrap();
    std::fs::create_dir(root.path().join("secrets")).unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let secret_path = canonical.join("secrets").to_string_lossy().into_owned();
    let tool = FsListTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "" }),
            &DenySuffixes(&["/secrets"]),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert!(!outcome.is_error);
    assert_eq!(
        entry_names(&outcome.output),
        BTreeSet::from(["visible.txt".to_owned()])
    );
    // Sanity: the suffix really did target the entry we expect, not nothing.
    assert!(secret_path.ends_with("/secrets"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_entry_is_authorized_by_its_own_path_never_its_target() {
    let outside = tempdir().unwrap();
    std::fs::write(outside.path().join("target.txt"), "").unwrap();
    let root = tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("target.txt"), root.path().join("link"))
        .unwrap();
    let canonical = root.path().canonicalize().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();
    let authorizer = Recording::default();

    let outcome = tool
        .invoke(json!({ "path": "" }), &authorizer, &ExecutionContext::new())
        .await
        .unwrap();

    // The symlink is listed (it is a name inside the confined root, not something being
    // followed), and what was authorized is the symlink's own path...
    assert_eq!(
        entry_names(&outcome.output),
        BTreeSet::from(["link".to_owned()])
    );
    let asked = authorizer.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].1.as_str(),
        canonical.join("link").to_string_lossy()
    );
    // ...never the target outside the root, which was never resolved at all.
    assert!(!asked[0].1.as_str().contains("target.txt"));
}

// ---------------------------------------------------------------------------
// Escaping the root
// ---------------------------------------------------------------------------

#[tokio::test]
async fn absolute_paths_are_rejected_even_when_they_point_inside_the_root() {
    let root = tempdir().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": root.path().to_str().unwrap() }),
            &Recording::default(),
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
    let tool = FsListTool::new(&root_dir).unwrap();

    let error = tool
        .invoke(
            json!({ "path": ".." }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_listing_target_that_escapes_the_root_is_rejected() {
    let outside = tempdir().unwrap();
    std::fs::create_dir(outside.path().join("stuff")).unwrap();

    let root = tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("stuff"), root.path().join("link")).unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "link" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Confinement(_)), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_listing_target_that_stays_within_the_root_is_followed_normally() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join("real")).unwrap();
    std::fs::write(root.path().join("real/inside.txt"), "").unwrap();
    std::os::unix::fs::symlink(root.path().join("real"), root.path().join("link")).unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let outcome = tool
        .invoke(
            json!({ "path": "link" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        entry_names(&outcome.output),
        BTreeSet::from(["inside.txt".to_owned()])
    );
}

// ---------------------------------------------------------------------------
// Ordinary filesystem conditions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_nonexistent_directory_is_reported_as_not_found() {
    let root = tempdir().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "ghost" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound { .. }), "{error}");
}

#[tokio::test]
async fn a_regular_file_is_rejected_as_not_a_directory() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hi").unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "notes.md" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn a_directory_over_the_configured_entry_limit_is_reported_as_a_model_visible_error() {
    let root = tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(root.path().join(format!("f{i}.txt")), "").unwrap();
    }
    let tool = FsListTool::new(root.path()).unwrap().with_max_entries(3);

    let outcome = tool
        .invoke(
            json!({ "path": "" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("3-entry")
    );
}

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_path_with_a_nul_byte_is_rejected() {
    let root = tempdir().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "a\0b" }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn a_non_string_path_field_is_a_structured_error() {
    let root = tempdir().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let error = tool
        .invoke(
            json!({ "path": 42 }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn extra_arguments_claiming_authority_have_no_effect() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    let tool = FsListTool::new(&root_dir).unwrap();

    let error = tool
        .invoke(
            json!({ "path": "..", "authorized": true, "i_am_the_owner": true }),
            &Recording::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

// ---------------------------------------------------------------------------
// Cancellation and deadlines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_call_cancelled_before_it_starts_is_reported_promptly() {
    let root = tempdir().unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let cx = ExecutionContext::new();
    cx.cancellation.cancel();

    let started = Instant::now();
    let error = tool
        .invoke(json!({ "path": "" }), &Recording::default(), &cx)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_deadline_already_in_the_past_times_out_without_scanning_anything() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hi").unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let cx = ExecutionContext::new().with_deadline(Timestamp::from_millis(0));
    let result = tool
        .invoke(json!({ "path": "" }), &Recording::default(), &cx)
        .await;
    if let Err(error) = result {
        assert!(matches!(error, Error::Timeout(_)), "{error}");
    }
}

#[tokio::test]
async fn cancellation_during_a_pending_entry_authorization_stops_the_call() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hi").unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let cx = ExecutionContext::new();
    let cancellation = cx.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancellation.cancel();
    });

    let started = Instant::now();
    let error = tool
        .invoke(json!({ "path": "" }), &HangForever, &cx)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_pending_entry_authorization_past_the_timeout_times_out() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hi").unwrap();
    let tool = FsListTool::new(root.path())
        .unwrap()
        .with_timeout(Duration::from_millis(50));

    let started = Instant::now();
    let error = tool
        .invoke(
            json!({ "path": "" }),
            &HangForever,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Timeout(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_generous_deadline_does_not_interfere_with_an_ordinary_listing() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hi").unwrap();
    let tool = FsListTool::new(root.path()).unwrap();

    let deadline = Timestamp::from_millis(
        (std::time::SystemTime::now() + Duration::from_secs(30))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    );
    let cx = ExecutionContext::new().with_deadline(deadline);

    let outcome = tool
        .invoke(json!({ "path": "" }), &Recording::default(), &cx)
        .await
        .unwrap();
    assert_eq!(
        entry_names(&outcome.output),
        BTreeSet::from(["notes.md".to_owned()])
    );
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_nonexistent_root_fails_at_construction_not_at_first_call() {
    let missing = tempdir().unwrap().path().join("ghost");
    let error = FsListTool::new(&missing).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config);
}

#[tokio::test]
async fn a_root_that_is_a_file_not_a_directory_fails_at_construction() {
    let root = tempdir().unwrap();
    let file = root.path().join("notes.md");
    std::fs::write(&file, "hi").unwrap();
    let error = FsListTool::new(&file).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config);
}
