//! One configuration file, a real tool server, and the gate everything goes through.
//!
//! `aik-mcp` has its own suite, and it proves the protocol: framing, refusals, bounds, what a
//! server may and may not ask for. What it cannot prove from inside itself is the claim this
//! file is about — that a server described in a deployment's *configuration* becomes a tool in
//! the same registry, behind the same policy engine, as a filesystem tool, and that the four
//! limits over it are wired in the order the documentation says.
//!
//! That claim spans `aik-runtime` (which reads the section and assembles), `aik-tools` (which
//! registers and authorizes), `aik-policy` (which decides) and `aik-mcp` (which serves), so it
//! belongs to none of them.
//!
//! The server is a shell script, started the way any other would be: found on a configured
//! search path, given an environment built from nothing, spoken to over pipes.

use std::path::Path;

use aik_api::execution::ExecutionContext;
use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::component::{Component, ComponentDescriptor};
use aik_core::id::ComponentId;
use aik_core::{ErrorKind, KernelBuilder};
use aik_runtime::{Deployment, McpSet, RuntimeSettings, ToolSet};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Writes a tool server offering `read_note` and `write_note`.
fn write_server(directory: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let script = r#"#!/bin/sh
tools='[{"name":"read_note","description":"Reads the note","inputSchema":{"type":"object"}},{"name":"write_note","description":"Writes the note","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"notes","version":"0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":%s}}\n' "$id" "$tools"
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"the note"}]}}\n' "$id"
      ;;
  esac
done
"#;

    let path = directory.join("notes-mcp");
    std::fs::write(&path, script).expect("a server script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("an executable server script");
}

/// The `agent.mcp` section a deployment would write, pointing at the scripted server.
///
/// `PATH` is named explicitly because a server inherits nothing: the script's own `sed` is
/// found only because this says where to look.
fn mcp_section(directory: &Path, tools: Value) -> Value {
    json!({
        "servers": [{
            "label": "notes",
            "command": "notes-mcp",
            "search_path": directory.display().to_string(),
            "env": { "PATH": "/usr/bin:/bin" },
            "tools": tools,
        }]
    })
}

/// A policy allowing `mcp.invoke` on one resource and refusing it on another.
///
/// The first rule answers the capability-level question — a rule with no `resource` answers
/// only that one — and the next two answer the resource-level question the tool's own claim
/// asks. Both phases have to pass, which is the whole point of naming the resource
/// `mcp:<server>/<tool>`.
fn policy() -> Value {
    json!({
        "rules": [
            { "action": "mcp.invoke", "effect": { "decision": "allow" } },
            { "action": "mcp.invoke", "resource": "mcp:notes/read_note",
              "effect": { "decision": "allow" } },
            { "action": "mcp.invoke", "resource": "mcp:notes/write_note",
              "effect": { "decision": "deny", "reason": "this deployment does not write notes" } }
        ]
    })
}

/// A policy that allows every question anybody asks it.
fn allow_everything() -> Value {
    json!({
        "rules": [
            { "action": "*", "effect": { "decision": "allow" } },
            { "action": "*", "resource": "*", "effect": { "decision": "allow" } }
        ]
    })
}

/// Resolves a deployment from `config`, as a frontend would, and asks for external tools.
fn settings(directory: &Path, config: Value) -> RuntimeSettings {
    let path = directory.join("aik.json");
    std::fs::write(&path, config.to_string()).expect("a configuration file");

    let config = aik_runtime::load_config(Some(&path), None, Vec::<(String, String)>::new())
        .expect("the configuration loads");

    Deployment {
        root: Some(directory.to_owned()),
        tools: ToolSet::None,
        memory: aik_runtime::MemorySet::Off,
        mcp: McpSet::On,
        storage: aik_runtime::StorageChoice::None,
        ..Deployment::default()
    }
    .resolve(config, Vec::<(String, String)>::new())
    .expect("the deployment resolves")
}

/// Stands in for the model provider the agent depends on.
///
/// Nothing here sends a turn to a model: every assertion is about which tools exist and who
/// may call them, which is settled before a model is ever asked anything. So this answers the
/// one question startup asks — is there a `dyn ModelProvider`? — and refuses the rest.
struct StubProvider;

#[async_trait]
impl aik_api::model::ModelProvider for StubProvider {
    async fn models(&self) -> aik_core::Result<Vec<aik_api::model::ModelDescriptor>> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        _request: aik_api::model::CompletionRequest,
        _cx: &ExecutionContext,
    ) -> aik_core::Result<aik_api::model::CompletionResponse> {
        Err(aik_core::Error::Unsupported(
            "this test never sends a turn to a model".into(),
        ))
    }

    async fn stream(
        &self,
        _request: aik_api::model::CompletionRequest,
        _cx: &ExecutionContext,
    ) -> aik_core::Result<
        futures::stream::BoxStream<'static, aik_core::Result<aik_api::model::CompletionChunk>>,
    > {
        Err(aik_core::Error::Unsupported(
            "this test never sends a turn to a model".into(),
        ))
    }
}

#[async_trait]
impl Component for StubProvider {
    fn descriptor(&self) -> ComponentDescriptor {
        ComponentDescriptor::new(ComponentId::new("model.stub"))
    }

    async fn init(&self, ctx: &aik_core::context::ComponentContext) -> aik_core::Result<()> {
        ctx.provide_default::<dyn aik_api::model::ModelProvider>(std::sync::Arc::new(StubProvider))
    }
}

/// Assembles the deployment's kernel with a provider that needs no server.
fn assemble(mut settings: RuntimeSettings) -> KernelBuilder {
    settings.model_component = ComponentId::new("model.stub");
    let (builder, _broker) = aik_runtime::builder(&settings, aik_api::model::ModelId::new("stub"))
        .expect("the deployment wires up");
    builder.component(StubProvider)
}

#[tokio::test]
async fn a_server_named_in_configuration_becomes_tools_in_the_registry() {
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path());

    let settings = settings(
        directory.path(),
        json!({
            "policy": policy(),
            "agent": { "mcp": mcp_section(directory.path(), json!([])) },
        }),
    );
    let kernel = assemble(settings).build().expect("a kernel");
    kernel.start().await.expect("the kernel starts");

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let names: Vec<String> = registry
        .list(&ExecutionContext::new())
        .await
        .expect("a listing")
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect();
    assert_eq!(names, ["mcp.notes.read_note", "mcp.notes.write_note"]);

    kernel.shutdown().await.expect("the kernel stops");
}

#[tokio::test]
async fn the_shipped_gate_decides_a_remote_call_exactly_as_it_decides_a_local_one() {
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path());

    let settings = settings(
        directory.path(),
        json!({
            "policy": policy(),
            "agent": { "mcp": mcp_section(directory.path(), json!([])) },
        }),
    );
    let kernel = assemble(settings).build().expect("a kernel");
    kernel.start().await.expect("the kernel starts");

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let cx = ExecutionContext::new();

    let allowed = registry
        .invoke(&ToolName::new("mcp.notes.read_note"), json!({}), &cx)
        .await
        .expect("the allowed tool runs");
    assert_eq!(allowed.output["text"], json!("the note"));

    let refused = registry
        .invoke(&ToolName::new("mcp.notes.write_note"), json!({}), &cx)
        .await
        .expect_err("the refused tool does not");
    assert_eq!(refused.kind(), ErrorKind::Permission);
    assert!(
        format!("{refused}").contains("does not write notes"),
        "{refused}"
    );

    kernel.shutdown().await.expect("the kernel stops");
}

#[tokio::test]
async fn a_deployments_allowlist_is_the_outer_limit_and_policy_cannot_widen_it() {
    // The tool refused here is one the policy above *allows*. What stops it is that the
    // deployment never registered it — the limit no rule can reach around.
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path());

    let settings = settings(
        directory.path(),
        json!({
            "policy": allow_everything(),
            "agent": { "mcp": mcp_section(directory.path(), json!(["read_note"])) },
        }),
    );
    let kernel = assemble(settings).build().expect("a kernel");
    kernel.start().await.expect("the kernel starts");

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = registry
        .invoke(
            &ToolName::new("mcp.notes.write_note"),
            json!({}),
            &ExecutionContext::new(),
        )
        .await
        .expect_err("a tool that was never registered");
    assert_eq!(error.kind(), ErrorKind::NotFound);

    kernel.shutdown().await.expect("the kernel stops");
}

#[tokio::test]
async fn a_run_that_did_not_ask_for_external_tools_has_none_however_configured() {
    // The frontend's own limit, outside both the allowlist and the policy: a deployment can
    // describe every server it likes, and a run that did not say `--mcp on` starts none.
    let directory = tempfile::tempdir().unwrap();
    write_server(directory.path());

    let path = directory.path().join("aik.json");
    std::fs::write(
        &path,
        json!({
            "policy": allow_everything(),
            "agent": { "mcp": mcp_section(directory.path(), json!([])) },
        })
        .to_string(),
    )
    .unwrap();
    let config = aik_runtime::load_config(Some(&path), None, Vec::<(String, String)>::new())
        .expect("the configuration loads");

    let settings = Deployment {
        root: Some(directory.path().to_owned()),
        tools: ToolSet::None,
        memory: aik_runtime::MemorySet::Off,
        storage: aik_runtime::StorageChoice::None,
        ..Deployment::default()
    }
    .resolve(config, Vec::<(String, String)>::new())
    .expect("the deployment resolves");
    assert_eq!(settings.mcp, McpSet::Off);

    let kernel = assemble(settings).build().expect("a kernel");
    kernel.start().await.expect("the kernel starts");

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    assert!(
        registry
            .list(&ExecutionContext::new())
            .await
            .unwrap()
            .is_empty(),
        "a run that did not ask for external tools got some"
    );

    kernel.shutdown().await.expect("the kernel stops");
}

#[tokio::test]
async fn a_misspelled_key_in_the_section_stops_the_deployment() {
    // `deny_unknown_fields`, all the way down. A `commmand` that was ignored would be a
    // server nobody could start and a setting nobody could find.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aik.json");
    std::fs::write(
        &path,
        json!({ "agent": { "mcp": { "servers": [{ "label": "notes", "commmand": "notes-mcp" }] } } })
            .to_string(),
    )
    .unwrap();
    let config = aik_runtime::load_config(Some(&path), None, Vec::<(String, String)>::new())
        .expect("the configuration loads");

    let error = Deployment {
        root: Some(directory.path().to_owned()),
        mcp: McpSet::On,
        storage: aik_runtime::StorageChoice::None,
        ..Deployment::default()
    }
    .resolve(config, Vec::<(String, String)>::new())
    .expect_err("a misspelled key is a startup failure");
    assert_eq!(error.kind(), ErrorKind::Config);
}
