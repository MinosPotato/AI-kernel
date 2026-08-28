//! End to end against a real child process speaking the protocol over pipes.
//!
//! The unit tests in `session` prove the framing rules against an in-memory peer. These
//! prove the part that only a process can: that a program is found on a configured search
//! path, started with an environment built from nothing, handshaken with, listed, called,
//! and killed — and that everything it offers arrives at a model through the same
//! authorization gate as a native tool.
//!
//! The server is a shell script rather than a Rust binary so that the test exercises the
//! actual spawn path — resolution, `env_clear`, pipes, the process group — instead of a
//! fixture that could quietly bypass it.

use std::sync::Arc;

use aik_api::execution::ExecutionContext;
use aik_api::permission::{Decision, PermissionRequest, PolicyEngine};
use aik_api::tool::{ToolCatalog, ToolName, ToolRegistry};
use aik_core::ErrorKind;
use aik_core::prelude::*;
use aik_mcp::{McpCatalog, McpClient, McpComponent, ServerSettings};
use aik_tools::ToolsComponent;

/// The tool list the script serves unless a test asks for another.
const TOOLS: &str = r#"[{"name":"greet","description":"Greets somebody","inputSchema":{"type":"object","properties":{"who":{"type":"string"}},"required":["who"]}},{"name":"forbidden","description":"Does something else","inputSchema":{"type":"object"}}]"#;

/// Writes an MCP server that answers the three methods this client uses.
///
/// `tools` is the JSON array served by `tools/list`, spliced in as a single-quoted shell
/// string so a test can serve a listing this crate should refuse without the script's own
/// quoting deciding what arrives.
fn write_server(directory: &std::path::Path, name: &str, tools: &str) {
    use std::os::unix::fs::PermissionsExt as _;

    assert!(
        !tools.contains('\''),
        "the script splices the tool list into a single-quoted string"
    );

    let script = format!(
        r#"#!/bin/sh
tools='{tools}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"scripted","version":"0"}}}}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":%s}}}}\n' "$id" "$tools"
      ;;
    *'"method":"tools/call"'*)
      who=$(printf '%s' "$line" | sed -n 's/.*"who":"\([^"]*\)".*/\1/p')
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"hello %s"}}]}}}}\n' "$id" "$who"
      ;;
  esac
done
"#
    );

    let path = directory.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Settings pointing at a script in `directory`.
///
/// `PATH` is named explicitly because nothing is inherited: the script's own `sed` would
/// otherwise not be found, which is exactly the property being demonstrated.
fn settings(directory: &std::path::Path, label: &str, command: &str) -> ServerSettings {
    let mut settings = ServerSettings {
        label: label.to_owned(),
        command: command.to_owned(),
        search_path: Some(directory.display().to_string()),
        ..ServerSettings::default()
    };
    settings.env.insert("PATH".into(), "/usr/bin:/bin".into());
    settings
}

/// A catalogue over one scripted server.
fn catalog(directory: &std::path::Path, settings: ServerSettings) -> Arc<McpCatalog> {
    let resolved = settings.resolve(directory).expect("settings resolve");
    Arc::new(McpCatalog::new(vec![Arc::new(McpClient::new(resolved))]).expect("a catalogue"))
}

#[tokio::test]
async fn a_scripted_server_is_started_listed_and_called() {
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path(), "scripted-mcp", TOOLS);
    let catalog = catalog(
        directory.path(),
        settings(directory.path(), "demo", "scripted-mcp"),
    );
    let cx = ExecutionContext::new();

    let specs = catalog.list(&cx).await.expect("a listing");
    let names: Vec<String> = specs.iter().map(|spec| spec.name.to_string()).collect();
    assert_eq!(names, ["mcp.demo.forbidden", "mcp.demo.greet"]);

    let greet = specs
        .iter()
        .find(|spec| spec.name.as_str() == "mcp.demo.greet")
        .unwrap();
    assert_eq!(greet.description, "Greets somebody");
    assert_eq!(greet.input_schema["properties"]["who"]["type"], "string");
    assert_eq!(
        greet.required_permissions,
        vec![aik_api::permission::ActionId::new(
            aik_mcp::DEFAULT_PERMISSION
        )]
    );
    // Never taken from the server: a tool that could declare itself read-only could declare
    // itself auto-approvable.
    assert!(!greet.read_only);

    catalog.shutdown().await;
}

#[tokio::test]
async fn the_deployments_allowlist_narrows_what_a_server_offers() {
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path(), "scripted-mcp", TOOLS);

    let mut raw = settings(directory.path(), "demo", "scripted-mcp");
    raw.tools = vec!["greet".into()];
    let catalog = catalog(directory.path(), raw);

    let specs = catalog.list(&ExecutionContext::new()).await.unwrap();
    let names: Vec<String> = specs.iter().map(|spec| spec.name.to_string()).collect();
    assert_eq!(names, ["mcp.demo.greet"]);

    catalog.shutdown().await;
}

#[tokio::test]
async fn an_allowlisted_tool_the_server_does_not_offer_is_a_configuration_failure() {
    // The failure this rules out is a typo in the allowlist reading as a capability the
    // deployment granted and the model silently never having.
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path(), "scripted-mcp", TOOLS);

    let mut raw = settings(directory.path(), "demo", "scripted-mcp");
    raw.tools = vec!["greeet".into()];
    let catalog = catalog(directory.path(), raw);

    let error = catalog
        .list(&ExecutionContext::new())
        .await
        .expect_err("a listing naming the tool that is not there");
    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(format!("{error}").contains("greeet"), "{error}");

    catalog.shutdown().await;
}

#[tokio::test]
async fn a_server_offering_an_unusable_name_fails_the_whole_listing() {
    let directory = tempfile::tempdir().unwrap();
    write_server(
        directory.path(),
        "scripted-mcp",
        r#"[{"name":"fs.write","inputSchema":{"type":"object"}}]"#,
    );
    let catalog = catalog(
        directory.path(),
        settings(directory.path(), "demo", "scripted-mcp"),
    );

    let error = catalog
        .list(&ExecutionContext::new())
        .await
        .expect_err("a refusal");
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    catalog.shutdown().await;
}

#[tokio::test]
async fn a_command_that_is_not_on_the_search_path_names_the_setting() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = catalog(
        directory.path(),
        settings(directory.path(), "demo", "absent-mcp"),
    );

    let error = catalog
        .list(&ExecutionContext::new())
        .await
        .expect_err("a failure to start");
    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(
        format!("{error}").contains("mcp.servers[demo].command"),
        "{error}"
    );
}

/// Allows everything, so a test can be about reachability rather than about rules.
struct AllowEverything;

#[async_trait]
impl PolicyEngine for AllowEverything {
    async fn evaluate(&self, _: &PermissionRequest, _: &ExecutionContext) -> Result<Decision> {
        Ok(Decision::Allow)
    }
}

/// Refuses the one resource it was built for, and allows the rest.
struct DenyResource(&'static str);

#[async_trait]
impl PolicyEngine for DenyResource {
    async fn evaluate(
        &self,
        request: &PermissionRequest,
        _: &ExecutionContext,
    ) -> Result<Decision> {
        match request.resource.as_ref() {
            Some(resource) if resource.as_str() == self.0 => Ok(Decision::Deny {
                reason: "not in this deployment".into(),
            }),
            _ => Ok(Decision::Allow),
        }
    }
}

/// Builds a kernel whose only tools come from a scripted MCP server.
async fn kernel_with(
    directory: &std::path::Path,
    policy: Arc<dyn PolicyEngine>,
) -> (Kernel, Arc<McpCatalog>) {
    write_server(directory, "scripted-mcp", TOOLS);
    let catalog = catalog(directory, settings(directory, "demo", "scripted-mcp"));

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_policy(policy)
                .with_catalog(catalog.clone() as Arc<dyn ToolCatalog>),
        )
        .component(McpComponent::new(catalog.clone()))
        .build()
        .expect("a kernel");
    kernel.start().await.expect("the kernel starts");
    (kernel, catalog)
}

#[tokio::test]
async fn a_remote_tool_is_reached_only_through_the_registrys_gate() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, _catalog) = kernel_with(directory.path(), Arc::new(AllowEverything)).await;

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let outcome = registry
        .invoke(
            &ToolName::new("mcp.demo.greet"),
            serde_json::json!({ "who": "world" }),
            &ExecutionContext::new(),
        )
        .await
        .expect("the call goes through");

    assert_eq!(outcome.output["text"], serde_json::json!("hello world"));
    assert!(!outcome.is_error);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn policy_refuses_one_of_a_servers_tools_without_touching_the_other() {
    // The point of naming the resource `mcp:<server>/<tool>`: a deployment can allow one
    // tool of a server and refuse another, with a rule written before either was listed.
    let directory = tempfile::tempdir().unwrap();
    let (kernel, _catalog) = kernel_with(
        directory.path(),
        Arc::new(DenyResource("mcp:demo/forbidden")),
    )
    .await;

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = ExecutionContext::new();

    let denied = registry
        .invoke(
            &ToolName::new("mcp.demo.forbidden"),
            serde_json::json!({}),
            &cx,
        )
        .await
        .expect_err("a refusal");
    assert_eq!(denied.kind(), ErrorKind::Permission);

    registry
        .invoke(
            &ToolName::new("mcp.demo.greet"),
            serde_json::json!({ "who": "world" }),
            &cx,
        )
        .await
        .expect("the other tool is unaffected");

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deployment_with_no_policy_cannot_call_a_remote_tool_at_all() {
    // Fail closed: an MCP tool declares a permission, and a registry with no policy engine
    // denies every permission it is asked about.
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path(), "scripted-mcp", TOOLS);
    let catalog = catalog(
        directory.path(),
        settings(directory.path(), "demo", "scripted-mcp"),
    );

    let kernel = Kernel::builder()
        .component(ToolsComponent::new().with_catalog(catalog.clone() as Arc<dyn ToolCatalog>))
        .component(McpComponent::new(catalog.clone()))
        .build()
        .unwrap();
    kernel.start().await.unwrap();

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = registry
        .invoke(
            &ToolName::new("mcp.demo.greet"),
            serde_json::json!({ "who": "world" }),
            &ExecutionContext::new(),
        )
        .await
        .expect_err("a denial");
    assert_eq!(error.kind(), ErrorKind::Permission);

    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutting_the_kernel_down_stops_the_server() {
    // The failure this rules out is a kernel that exits and leaves a tool server running,
    // holding whatever it had open.
    let directory = tempfile::tempdir().unwrap();
    let command = format!(
        "scripted-mcp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    write_server(directory.path(), &command, TOOLS);
    let catalog = catalog(directory.path(), settings(directory.path(), "demo", &command));

    let kernel = Kernel::builder()
        .component(
            ToolsComponent::new()
                .with_policy(Arc::new(AllowEverything))
                .with_catalog(catalog.clone() as Arc<dyn ToolCatalog>),
        )
        .component(McpComponent::new(catalog))
        .build()
        .expect("a kernel");
    kernel.start().await.expect("the kernel starts");
    kernel.shutdown().await.unwrap();

    // The script's own children are in the process group the child led, so a `sed` mid-call
    // goes with it. What is checked here is the part that is observable without racing the
    // scheduler: the session is gone, so the next call cannot be answered by the old process.
    let pids = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("cmdline"))
                .map(|cmdline| cmdline.contains(&command))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(pids, 0, "an MCP server outlived the kernel that started it");
}

#[tokio::test]
async fn a_non_object_argument_is_refused_before_it_reaches_the_server() {
    let directory = tempfile::tempdir().unwrap();
    let (kernel, _catalog) = kernel_with(directory.path(), Arc::new(AllowEverything)).await;

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = registry
        .invoke(
            &ToolName::new("mcp.demo.greet"),
            serde_json::json!("who=world"),
            &ExecutionContext::new(),
        )
        .await
        .expect_err("an argument error the model can correct");
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    kernel.shutdown().await.unwrap();
}
