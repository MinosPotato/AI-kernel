//! Security tests for [`FsWriteTool`], exercised directly against the `Tool` trait.
//!
//! These use a real temporary directory and real filesystem calls — no mocking — because
//! the property under test (does confinement actually hold against the OS) is exactly the
//! kind of thing a mock would beg the question on. Every test cleans up via `tempfile`'s
//! `Drop` impl; nothing here touches the real filesystem outside a fresh temp directory.
//!
//! A mutating tool needs one assertion a reading tool does not: that a refusal left the
//! host *unchanged*. Almost every negative test below therefore checks both that the call
//! failed and that nothing was created, truncated or overwritten.

use std::path::Path;
use std::time::Duration;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::Tool;
use aik_core::clock::Timestamp;
use aik_core::{Error, ErrorKind, Result};
use aik_fs::FsWriteTool;
use async_trait::async_trait;
use serde_json::json;
use tempfile::tempdir;

/// [`FsWriteTool`] declares the resource it writes up front and refuses anything that would
/// take it elsewhere, so it never needs to ask about a resource discovered mid-run. This
/// authorizer panics if that assumption is ever violated.
struct MustNotBeAsked;

#[async_trait]
impl ResourceAuthorizer for MustNotBeAsked {
    async fn authorize(&self, _action: &ActionId, resource: &ResourceId) -> Result<()> {
        panic!(
            "FsWriteTool asked about a discovered resource `{resource}`; it should never need to"
        )
    }
}

async fn write(
    tool: &FsWriteTool,
    path: &str,
    content: &str,
) -> Result<aik_api::tool::ToolOutcome> {
    tool.invoke(
        json!({ "path": path, "content": content }),
        &MustNotBeAsked,
        &ExecutionContext::new(),
    )
    .await
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap()
}

// ---------------------------------------------------------------------------
// Normal writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_permitted_write_creates_the_file() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let outcome = write(&tool, "notes.md", "hello, world").await.unwrap();

    assert!(!outcome.is_error);
    assert_eq!(outcome.output["path"], json!("notes.md"));
    assert_eq!(outcome.output["bytes_written"], json!(12));
    assert_eq!(read(root.path().join("notes.md")), "hello, world");
}

#[tokio::test]
async fn a_write_in_a_nested_existing_directory_succeeds() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    write(&tool, "src/lib.rs", "fn main() {}").await.unwrap();
    assert_eq!(read(root.path().join("src/lib.rs")), "fn main() {}");
}

#[tokio::test]
async fn replacing_a_file_leaves_no_tail_of_the_previous_contents() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "aaaaaaaaaaaaaaaaaaaa").unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    write(&tool, "notes.md", "bb").await.unwrap();
    assert_eq!(read(root.path().join("notes.md")), "bb");
}

#[tokio::test]
async fn an_empty_write_truncates_the_file() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "previous").unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let outcome = write(&tool, "notes.md", "").await.unwrap();
    assert_eq!(outcome.output["bytes_written"], json!(0));
    assert_eq!(read(root.path().join("notes.md")), "");
}

#[tokio::test]
async fn multibyte_content_is_written_as_utf8_and_counted_in_bytes() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let outcome = write(&tool, "notes.md", "héllo").await.unwrap();
    assert_eq!(outcome.output["bytes_written"], json!(6));
    assert_eq!(read(root.path().join("notes.md")), "héllo");
}

// ---------------------------------------------------------------------------
// Escaping the root: absolute paths, `..`, and symlinks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn absolute_paths_are_rejected_even_when_they_point_inside_the_root() {
    let root = tempdir().unwrap();
    let target = root.path().join("notes.md");
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, target.to_str().unwrap(), "x")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert!(!target.exists());
}

#[tokio::test]
async fn parent_directory_traversal_is_rejected_and_writes_nothing() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();
    let tool = FsWriteTool::new(&root_dir).unwrap();

    let error = write(&tool, "../secret.txt", "OVERWRITTEN")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(read(outer.path().join("secret.txt")), "TOP SECRET");
}

#[tokio::test]
async fn traversal_buried_inside_a_longer_path_is_also_rejected() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir_all(root_dir.join("sub")).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "TOP SECRET").unwrap();
    let tool = FsWriteTool::new(&root_dir).unwrap();

    let error = write(&tool, "sub/../../secret.txt", "OVERWRITTEN")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(read(outer.path().join("secret.txt")), "TOP SECRET");
}

#[tokio::test]
async fn a_single_dot_component_is_rejected() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "./notes.md", "x").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert!(!root.path().join("notes.md").exists());
}

#[tokio::test]
async fn an_empty_path_is_rejected() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "", "x").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[tokio::test]
async fn an_embedded_nul_byte_is_rejected() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "notes\0.md", "x").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_target_pointing_outside_the_root_is_refused() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    let secret = outer.path().join("secret.txt");
    std::fs::write(&secret, "TOP SECRET").unwrap();
    std::os::unix::fs::symlink(&secret, root_dir.join("link")).unwrap();
    let tool = FsWriteTool::new(&root_dir).unwrap();

    let error = write(&tool, "link", "OVERWRITTEN").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(read(&secret), "TOP SECRET");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_target_pointing_inside_the_root_is_refused_too() {
    // Even a link that stays inside the root is refused: the point is that the path policy
    // authorized and the object written must be the same object, and a symlink makes them
    // two different things.
    let root = tempdir().unwrap();
    let real = root.path().join("real.txt");
    std::fs::write(&real, "original").unwrap();
    std::os::unix::fs::symlink(&real, root.path().join("link")).unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "link", "OVERWRITTEN").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(read(&real), "original");
}

#[cfg(unix)]
#[tokio::test]
async fn a_dangling_symlink_target_is_refused_rather_than_created_through() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    let missing = outer.path().join("not-there.txt");
    std::os::unix::fs::symlink(&missing, root_dir.join("link")).unwrap();
    let tool = FsWriteTool::new(&root_dir).unwrap();

    let error = write(&tool, "link", "CREATED").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert!(!missing.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_parent_directory_escaping_the_root_is_refused() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    let elsewhere = outer.path().join("elsewhere");
    std::fs::create_dir(&root_dir).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root_dir.join("escape")).unwrap();
    let tool = FsWriteTool::new(&root_dir).unwrap();

    let error = write(&tool, "escape/planted.txt", "PLANTED")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert!(!elsewhere.join("planted.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_parent_directory_inside_the_root_resolves_and_is_allowed() {
    let root = tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, root.path().join("link")).unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    write(&tool, "link/notes.md", "hello").await.unwrap();
    assert_eq!(read(real.join("notes.md")), "hello");

    // …and the resource policy was asked about is the resolved location, not the link.
    let claims = tool
        .planned_resources(&json!({ "path": "link/notes.md", "content": "hello" }))
        .unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(
        claims[0].resource.as_str(),
        real.canonicalize()
            .unwrap()
            .join("notes.md")
            .to_str()
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// Targets that are not plain, private regular files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn writing_onto_an_existing_directory_is_refused() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "src", "x").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert!(root.path().join("src").is_dir());
}

#[tokio::test]
async fn a_parent_that_is_a_file_rather_than_a_directory_is_refused() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "hello").unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "notes.md/child", "x").await.unwrap_err();
    assert_ne!(error.kind(), ErrorKind::Permission, "{error}");
    assert_eq!(read(root.path().join("notes.md")), "hello");
}

#[cfg(unix)]
#[tokio::test]
async fn a_special_file_at_the_target_is_refused() {
    let root = tempdir().unwrap();
    // A unix socket is a non-regular file any unprivileged test can create; a device node
    // or a FIFO would exercise the same refusal.
    let _listener = std::os::unix::net::UnixListener::bind(root.path().join("sock")).unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "sock", "x").await.unwrap_err();
    assert_ne!(error.kind(), ErrorKind::Permission, "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_hard_linked_file_is_refused_because_the_second_name_may_be_outside_the_root() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    let secret = outer.path().join("secret.txt");
    std::fs::write(&secret, "TOP SECRET").unwrap();
    std::fs::hard_link(&secret, root_dir.join("inside.txt")).unwrap();
    let tool = FsWriteTool::new(&root_dir).unwrap();

    let error = write(&tool, "inside.txt", "OVERWRITTEN").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(read(&secret), "TOP SECRET");
    assert_eq!(read(root_dir.join("inside.txt")), "TOP SECRET");
}

#[cfg(unix)]
#[tokio::test]
async fn a_hard_link_wholly_inside_the_root_is_refused_too() {
    // Conservative by design: nothing in a path-based check can tell this case apart from
    // the escaping one, so both are refused.
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "original").unwrap();
    std::fs::hard_link(root.path().join("a.txt"), root.path().join("b.txt")).unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "b.txt", "OVERWRITTEN").await.unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    assert_eq!(read(root.path().join("a.txt")), "original");
}

// ---------------------------------------------------------------------------
// Missing parents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_parent_directory_is_not_created() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let error = write(&tool, "missing/notes.md", "x").await.unwrap_err();
    assert!(matches!(error, Error::NotFound { .. }), "{error}");
    assert!(!root.path().join("missing").exists());
}

// ---------------------------------------------------------------------------
// Size limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_over_the_limit_is_a_model_visible_error_and_creates_nothing() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap().with_max_bytes(4);

    let outcome = write(&tool, "notes.md", "hello").await.unwrap();
    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("4-byte write limit")
    );
    assert!(!root.path().join("notes.md").exists());
}

#[tokio::test]
async fn content_over_the_limit_does_not_truncate_an_existing_file() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), "original").unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap().with_max_bytes(4);

    let outcome = write(&tool, "notes.md", "hello").await.unwrap();
    assert!(outcome.is_error);
    assert_eq!(read(root.path().join("notes.md")), "original");
}

#[tokio::test]
async fn content_exactly_at_the_limit_is_written() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap().with_max_bytes(4);

    let outcome = write(&tool, "notes.md", "abcd").await.unwrap();
    assert!(!outcome.is_error);
    assert_eq!(read(root.path().join("notes.md")), "abcd");
}

// ---------------------------------------------------------------------------
// Permissions of created files
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_created_file_is_not_readable_by_group_or_others() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    write(&tool, "notes.md", "secret-ish").await.unwrap();
    let mode = std::fs::metadata(root.path().join("notes.md"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
}

#[cfg(unix)]
#[tokio::test]
async fn an_existing_files_permissions_are_left_alone() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().unwrap();
    let path = root.path().join("notes.md");
    std::fs::write(&path, "original").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    write(&tool, "notes.md", "replaced").await.unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640, "mode was {mode:o}");
}

// ---------------------------------------------------------------------------
// Declared resources
// ---------------------------------------------------------------------------

#[test]
fn planned_resources_declares_the_canonical_target_path() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    let claims = tool
        .planned_resources(&json!({ "path": "notes.md", "content": "x" }))
        .unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].action.as_str(), aik_fs::DEFAULT_WRITE_PERMISSION);
    assert_eq!(
        claims[0].resource.as_str(),
        tool.root().join("notes.md").to_str().unwrap()
    );
}

#[test]
fn planned_resources_refuses_a_traversal_before_any_policy_question() {
    let outer = tempdir().unwrap();
    let root_dir = outer.path().join("root");
    std::fs::create_dir(&root_dir).unwrap();
    let tool = FsWriteTool::new(&root_dir).unwrap();

    let error = tool
        .planned_resources(&json!({ "path": "../secret.txt", "content": "x" }))
        .unwrap_err();
    assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
}

#[test]
fn planned_resources_does_not_create_anything() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    tool.planned_resources(&json!({ "path": "notes.md", "content": "x" }))
        .unwrap();
    assert!(!root.path().join("notes.md").exists());
}

// ---------------------------------------------------------------------------
// Specification
// ---------------------------------------------------------------------------

#[test]
fn the_spec_declares_a_mutating_tool_needing_the_write_permission() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();
    let spec = tool.spec();

    assert_eq!(spec.name.as_str(), aik_fs::DEFAULT_WRITE_NAME);
    assert!(!spec.read_only);
    assert_eq!(
        spec.required_permissions,
        vec![ActionId::new(aik_fs::DEFAULT_WRITE_PERMISSION)]
    );
    // Reading and writing are never the same capability.
    assert_ne!(aik_fs::DEFAULT_WRITE_PERMISSION, aik_fs::DEFAULT_PERMISSION);
}

#[test]
fn the_permission_can_be_overridden_without_changing_confinement() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path())
        .unwrap()
        .with_name("project.write")
        .with_permission("project.write");
    let spec = tool.spec();

    assert_eq!(spec.name.as_str(), "project.write");
    assert_eq!(
        spec.required_permissions,
        vec![ActionId::new("project.write")]
    );
    assert!(
        tool.planned_resources(&json!({ "path": "../x", "content": "y" }))
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Arguments and configuration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_arguments_are_rejected() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();

    for arguments in [
        json!({ "path": "notes.md" }),
        json!({ "content": "x" }),
        json!({}),
    ] {
        let error = tool
            .invoke(arguments, &MustNotBeAsked, &ExecutionContext::new())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }
    assert!(!root.path().join("notes.md").exists());
}

#[test]
fn a_root_that_does_not_exist_is_a_configuration_error() {
    let root = tempdir().unwrap();
    let error = FsWriteTool::new(root.path().join("nope")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
}

#[test]
fn a_root_that_is_a_file_is_a_configuration_error() {
    let root = tempdir().unwrap();
    let file = root.path().join("notes.md");
    std::fs::write(&file, "hello").unwrap();
    let error = FsWriteTool::new(&file).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
}

// ---------------------------------------------------------------------------
// Cancellation and deadlines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_already_cancelled_call_writes_nothing() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();
    let cx = ExecutionContext::new();
    cx.cancellation.cancel();

    let error = tool
        .invoke(
            json!({ "path": "notes.md", "content": "x" }),
            &MustNotBeAsked,
            &cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(!root.path().join("notes.md").exists());
}

#[tokio::test]
async fn an_already_expired_deadline_writes_nothing() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path()).unwrap();
    let cx = ExecutionContext::new().with_deadline(Timestamp::from_millis(1));

    let error = tool
        .invoke(
            json!({ "path": "notes.md", "content": "x" }),
            &MustNotBeAsked,
            &cx,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Timeout, "{error}");
    assert!(!root.path().join("notes.md").exists());
}

#[tokio::test]
async fn a_generous_deadline_still_permits_the_write() {
    let root = tempdir().unwrap();
    let tool = FsWriteTool::new(root.path())
        .unwrap()
        .with_timeout(Duration::from_secs(30));
    let deadline = Timestamp::from_millis(
        (std::time::SystemTime::now() + Duration::from_secs(30))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    );
    let cx = ExecutionContext::new().with_deadline(deadline);

    tool.invoke(
        json!({ "path": "notes.md", "content": "hello" }),
        &MustNotBeAsked,
        &cx,
    )
    .await
    .unwrap();
    assert_eq!(read(root.path().join("notes.md")), "hello");
}

// ---------------------------------------------------------------------------
// Independence from the read tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_write_tool_and_a_read_tool_on_the_same_root_stay_independent() {
    let root = tempdir().unwrap();
    let writer = FsWriteTool::new(root.path()).unwrap();
    let reader = aik_fs::FsReadTool::new(root.path()).unwrap();

    write(&writer, "notes.md", "written by the tool")
        .await
        .unwrap();
    let outcome = reader
        .invoke(
            json!({ "path": "notes.md" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.output["content"], json!("written by the tool"));

    // The read tool's spec cannot be mistaken for the write tool's.
    assert!(reader.spec().read_only);
    assert!(!writer.spec().read_only);
    assert_ne!(reader.spec().name, writer.spec().name);
}
