//! What [`WebFetchTool`] refuses, with the settings a deployment gets by default.
//!
//! Every test here runs against `NetSettings::default()` — `https` only, global addresses
//! only — or against the one relaxation that exists, and asserts two things where it can:
//! that the call was refused, and that nothing was sent. The second matters as much as the
//! first: a refusal that arrives after the request has already been made is not confinement,
//! it is a report.

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::{Tool, ToolOutcome};
use aik_core::{ErrorKind, Result};
use aik_net::{NetSettings, WebFetchTool};
use async_trait::async_trait;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct MustNotBeAsked;

#[async_trait]
impl ResourceAuthorizer for MustNotBeAsked {
    async fn authorize(&self, _action: &ActionId, resource: &ResourceId) -> Result<()> {
        panic!("asked about `{resource}` on a call that should have been refused")
    }
}

fn tool(settings: NetSettings) -> WebFetchTool {
    WebFetchTool::new(settings).expect("a buildable client")
}

async fn refusal(settings: NetSettings, url: &str) -> ToolOutcome {
    tool(settings)
        .invoke(
            json!({ "url": url }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .expect("a refusal the model can read, not a failed call")
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_loopback_url_is_refused_and_nothing_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
        .mount(&server)
        .await;

    // The server is on loopback over http, and both of those are off by default. `allow_http`
    // is on here so that the *address* check is what refuses, rather than the scheme check.
    let settings = NetSettings {
        allow_http: true,
        ..NetSettings::default()
    };
    let outcome = refusal(settings, &format!("{}/anything", server.uri())).await;

    assert!(outcome.is_error);
    let reason = outcome.output["error"].as_str().unwrap();
    assert!(reason.contains("127.0.0.0/8"), "{reason}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_instance_metadata_address_is_refused_even_with_local_addresses_allowed() {
    let settings = NetSettings {
        allow_http: true,
        allow_local_addresses: true,
        ..NetSettings::default()
    };
    let outcome = refusal(settings, "http://169.254.169.254/latest/meta-data/").await;

    assert!(outcome.is_error);
    let reason = outcome.output["error"].as_str().unwrap();
    assert!(reason.contains("link-local"), "{reason}");
}

#[tokio::test]
async fn an_ipv4_address_spelled_as_ipv6_is_refused_the_same_way() {
    let outcome = refusal(NetSettings::default(), "https://[::ffff:127.0.0.1]/x").await;
    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("127.0.0.0/8")
    );
}

#[tokio::test]
async fn a_private_range_address_is_refused_until_a_deployment_opts_in() {
    let outcome = refusal(NetSettings::default(), "https://10.1.2.3/admin").await;
    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("RFC 1918")
    );
}

// ---------------------------------------------------------------------------
// Shapes that never become a request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_scheme_that_is_not_http_is_refused_before_any_decision_is_made() {
    let tool = tool(NetSettings::default());
    for url in ["file:///etc/passwd", "ftp://example.com/x", "gopher://x/"] {
        // Refused in `planned_resources`, which runs before the registry authorizes
        // anything, so such a call never reaches a policy engine at all.
        let planned = tool.planned_resources(&json!({ "url": url }));
        assert_eq!(planned.unwrap_err().kind(), ErrorKind::Confinement, "{url}");

        let error = tool
            .invoke(
                json!({ "url": url }),
                &MustNotBeAsked,
                &ExecutionContext::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Confinement, "{url}");
    }
}

#[tokio::test]
async fn plaintext_is_refused_by_default() {
    let tool = tool(NetSettings::default());
    let error = tool
        .planned_resources(&json!({ "url": "http://example.com/" }))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Confinement);
}

#[tokio::test]
async fn a_url_carrying_credentials_is_refused() {
    let tool = tool(NetSettings::default());
    let error = tool
        .planned_resources(&json!({ "url": "https://user:pw@example.com/" }))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Confinement);
}

#[tokio::test]
async fn a_privileged_port_that_is_not_the_web_is_refused() {
    let tool = tool(NetSettings::default());
    let error = tool
        .planned_resources(&json!({ "url": "https://example.com:22/" }))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Confinement);
}

#[tokio::test]
async fn a_host_outside_the_allow_list_is_refused() {
    let settings = NetSettings {
        allow_hosts: vec![".rust-lang.org".to_owned()],
        ..NetSettings::default()
    };
    let error = tool(settings)
        .planned_resources(&json!({ "url": "https://example.com/" }))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Confinement);
}

#[tokio::test]
async fn arguments_the_schema_does_not_describe_are_refused() {
    // The one argument is the URL. A caller that tries to add a method or a header is not
    // silently given a fetch with the extra field ignored.
    let tool = tool(NetSettings::default());
    let error = tool
        .invoke(
            // An address that is refused anyway, so that a regression in the argument check
            // cannot turn this test into a real request to somebody else's server.
            json!({ "url": "https://10.1.2.3/", "method": "POST" }),
            &MustNotBeAsked,
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
}

// ---------------------------------------------------------------------------
// The caller's own bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_call_cancelled_before_it_starts_is_reported_promptly_and_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
        .mount(&server)
        .await;

    let settings = NetSettings {
        allow_http: true,
        allow_local_addresses: true,
        ..NetSettings::default()
    };
    let cx = ExecutionContext::new();
    cx.cancellation.cancel();

    let error = tool(settings)
        .invoke(
            json!({ "url": format!("{}/x", server.uri()) }),
            &MustNotBeAsked,
            &cx,
        )
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_deadline_that_has_passed_leaves_no_budget_to_fetch_with() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(5))
                .set_body_string("too late"),
        )
        .mount(&server)
        .await;

    let settings = NetSettings {
        allow_http: true,
        allow_local_addresses: true,
        ..NetSettings::default()
    };
    let cx = ExecutionContext::new().with_deadline(aik_core::clock::Timestamp::now());

    let error = tool(settings)
        .invoke(
            json!({ "url": format!("{}/x", server.uri()) }),
            &MustNotBeAsked,
            &cx,
        )
        .await
        .unwrap_err();
    assert_ne!(error.kind(), ErrorKind::Permission);
}
