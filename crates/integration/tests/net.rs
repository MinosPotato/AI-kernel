//! A fetch, from a deployment's configuration to a socket, through everything in between.
//!
//! `aik-net` has its own suite, and it proves the boundary: which URLs are refused, which
//! addresses are unreachable, what a redirect does. What it cannot prove from inside itself
//! is the claim this file is about — that the tool a deployment turns on with `--net on` is
//! registered in the same registry, behind the same policy engine, as a filesystem tool, and
//! that the resource claims it makes are the ones a rule is actually written against.
//!
//! One of those claims is unlike anything else in the workspace. A redirect is a resource
//! *discovered mid-run*: it did not exist when the call was authorized, because the server
//! that named it had not answered yet. `aik-net` can only assert that it asks; `aik-tools`
//! and `aik-policy` can only assert that they answer. That the question and the answer meet —
//! and that a hop the deployment's policy refuses is never fetched — spans all three.

use std::path::Path;

use aik_api::execution::ExecutionContext;
use aik_api::tool::{ToolName, ToolRegistry};
use aik_core::component::{Component, ComponentDescriptor};
use aik_core::id::ComponentId;
use aik_core::{ErrorKind, KernelBuilder};
use aik_runtime::{Deployment, NetSet, RuntimeSettings, ToolSet};
use async_trait::async_trait;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The `agent.net` section a deployment would write to reach a service on its own machine.
fn net_section() -> Value {
    json!({ "allow_http": true, "allow_local_addresses": true })
}

/// Allows fetching from the loopback address, and refuses the same server by its other name.
///
/// Two names for one machine is what makes this a policy test rather than an address test:
/// `127.0.0.1` and `localhost` resolve identically, every address check passes for both, and
/// the only thing that can tell them apart is a rule written about the host resource.
fn policy() -> Value {
    json!({
        "rules": [
            { "action": "web.fetch", "effect": { "decision": "allow" } },
            { "action": "web.fetch", "resource": "host/127.0.0.1",
              "effect": { "decision": "allow" } },
            { "action": "web.fetch", "resource": "url/http://127.0.0.1*",
              "effect": { "decision": "allow" } },
            { "action": "web.fetch", "resource": "host/localhost",
              "effect": { "decision": "deny", "reason": "this deployment does not fetch from localhost" } }
        ]
    })
}

/// Resolves a deployment from `config`, as a frontend would, and asks for the network.
fn settings(directory: &Path, config: Value, net: NetSet) -> RuntimeSettings {
    let path = directory.join("aik.json");
    std::fs::write(&path, config.to_string()).expect("a configuration file");

    let config = aik_runtime::load_config(Some(&path), None, Vec::<(String, String)>::new())
        .expect("the configuration loads");

    Deployment {
        root: Some(directory.to_owned()),
        tools: ToolSet::None,
        memory: aik_runtime::MemorySet::Off,
        net,
        storage: aik_runtime::StorageChoice::None,
        ..Deployment::default()
    }
    .resolve(config, Vec::<(String, String)>::new())
    .expect("the deployment resolves")
}

/// Stands in for the model provider the agent depends on; nothing here sends a turn.
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

fn assemble(mut settings: RuntimeSettings) -> KernelBuilder {
    settings.model_component = ComponentId::new("model.stub");
    let (builder, _broker) = aik_runtime::builder(&settings, aik_api::model::ModelId::new("stub"))
        .expect("the deployment wires up");
    builder.component(StubProvider)
}

#[tokio::test]
async fn the_fetch_tool_exists_only_in_a_run_that_asked_for_it() {
    let directory = tempfile::tempdir().unwrap();
    let configuration = json!({ "policy": policy(), "agent": { "net": net_section() } });

    for (net, expected) in [(NetSet::Off, false), (NetSet::On, true)] {
        let settings = settings(directory.path(), configuration.clone(), net);
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
        assert_eq!(names.contains(&"web.fetch".to_owned()), expected, "{net:?}");

        kernel.shutdown().await.expect("the kernel stops");
    }
}

#[tokio::test]
async fn a_fetch_the_deployments_policy_allows_reaches_the_server() {
    let directory = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/doc"))
        .respond_with(ResponseTemplate::new(200).set_body_string("the document"))
        .mount(&server)
        .await;

    let settings = settings(
        directory.path(),
        json!({ "policy": policy(), "agent": { "net": net_section() } }),
        NetSet::On,
    );
    let kernel = assemble(settings).build().expect("a kernel");
    kernel.start().await.expect("the kernel starts");

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let outcome = registry
        .invoke(
            &ToolName::new("web.fetch"),
            json!({ "url": format!("{}/doc", server.uri()) }),
            &ExecutionContext::new(),
        )
        .await
        .expect("the allowed fetch runs");

    assert!(!outcome.is_error);
    assert_eq!(outcome.output["content"], json!("the document"));

    kernel.shutdown().await.expect("the kernel stops");
}

#[tokio::test]
async fn a_host_the_policy_denies_is_refused_before_anything_is_sent() {
    let directory = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
        .mount(&server)
        .await;

    let settings = settings(
        directory.path(),
        json!({ "policy": policy(), "agent": { "net": net_section() } }),
        NetSet::On,
    );
    let kernel = assemble(settings).build().expect("a kernel");
    kernel.start().await.expect("the kernel starts");

    let port = server.address().port();
    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = registry
        .invoke(
            &ToolName::new("web.fetch"),
            json!({ "url": format!("http://localhost:{port}/doc") }),
            &ExecutionContext::new(),
        )
        .await
        .expect_err("a denied host does not run");

    assert_eq!(error.kind(), ErrorKind::Permission);
    assert!(
        format!("{error}").contains("does not fetch from localhost"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());

    kernel.shutdown().await.expect("the kernel stops");
}

#[tokio::test]
async fn a_redirect_to_a_denied_host_is_refused_by_the_policy_that_never_saw_it_coming() {
    // The property this whole file exists for. The call authorized at the start named
    // `127.0.0.1`; the destination the server then chose is a host the deployment refuses,
    // and the refusal has to happen against the *live* policy engine, mid-call, before the
    // second request is made.
    let directory = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let port = server.address().port();
    Mock::given(method("GET"))
        .and(path("/away"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("http://localhost:{port}/elsewhere").as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
        .mount(&server)
        .await;

    let settings = settings(
        directory.path(),
        json!({ "policy": policy(), "agent": { "net": net_section() } }),
        NetSet::On,
    );
    let kernel = assemble(settings).build().expect("a kernel");
    kernel.start().await.expect("the kernel starts");

    let registry = kernel.context().service::<dyn ToolRegistry>().unwrap();
    let error = registry
        .invoke(
            &ToolName::new("web.fetch"),
            json!({ "url": format!("http://127.0.0.1:{port}/away") }),
            &ExecutionContext::new(),
        )
        .await
        .expect_err("the hop is refused");

    assert_eq!(error.kind(), ErrorKind::Permission);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "the refused hop was fetched anyway");
    assert_eq!(requests[0].url.path(), "/away");

    kernel.shutdown().await.expect("the kernel stops");
}
