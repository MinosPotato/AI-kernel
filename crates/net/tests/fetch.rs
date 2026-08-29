//! What [`WebFetchTool`] does with a real HTTP server on the other end.
//!
//! The server is a `wiremock` instance on loopback, which means these tests run with
//! `allow_local_addresses` and `allow_http` on — the two settings a deployment turns on
//! deliberately. The confinement tests in `confinement.rs` are the ones that run with them
//! off, and between them they cover both sides of every switch.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aik_api::execution::ExecutionContext;
use aik_api::permission::{ActionId, ResourceAuthorizer, ResourceId};
use aik_api::tool::Tool;
use aik_core::{ErrorKind, Result};
use aik_net::{NetSettings, WebFetchTool};
use async_trait::async_trait;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The settings a deployment needs in order to reach a service on its own machine.
fn local() -> NetSettings {
    NetSettings {
        allow_http: true,
        allow_local_addresses: true,
        ..NetSettings::default()
    }
}

fn tool(settings: NetSettings) -> WebFetchTool {
    WebFetchTool::new(settings).expect("a buildable client")
}

/// Nothing is discovered mid-run on a call that does not redirect.
struct MustNotBeAsked;

#[async_trait]
impl ResourceAuthorizer for MustNotBeAsked {
    async fn authorize(&self, _action: &ActionId, resource: &ResourceId) -> Result<()> {
        panic!("asked about `{resource}` on a call that discovers nothing")
    }
}

/// Records every resource a redirect caused to be asked about.
#[derive(Default)]
struct Records {
    asked: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl ResourceAuthorizer for Records {
    async fn authorize(&self, _action: &ActionId, resource: &ResourceId) -> Result<()> {
        self.asked.lock().unwrap().push(resource.to_string());
        Ok(())
    }
}

/// Refuses everything, and counts how often it was consulted.
#[derive(Default)]
struct Refuses {
    asked: AtomicUsize,
}

#[async_trait]
impl ResourceAuthorizer for Refuses {
    async fn authorize(&self, _action: &ActionId, resource: &ResourceId) -> Result<()> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Err(aik_core::Error::PermissionDenied(format!(
            "`{resource}` is refused"
        )))
    }
}

async fn get(tool: &WebFetchTool, url: &str) -> aik_api::tool::ToolOutcome {
    tool.invoke(
        json!({ "url": url }),
        &MustNotBeAsked,
        &ExecutionContext::new(),
    )
    .await
    .expect("the call itself to succeed")
}

// ---------------------------------------------------------------------------
// Ordinary retrieval
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_html_page_comes_back_as_its_readable_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><head><title>T</title><style>b{}</style></head>\
                 <body><h1>Heading</h1><p>Body &amp; text</p>\
                 <script>var x=1</script></body></html>",
            "text/html; charset=utf-8",
        ))
        .mount(&server)
        .await;

    let outcome = get(&tool(local()), &format!("{}/page", server.uri())).await;

    assert!(!outcome.is_error);
    assert_eq!(outcome.output["status"], json!(200));
    let content = outcome.output["content"].as_str().unwrap();
    assert!(content.contains("Heading"), "{content}");
    assert!(content.contains("Body & text"), "{content}");
    assert!(!content.contains("var x"), "script survived: {content}");
    assert!(!content.contains("b{}"), "style survived: {content}");
    assert_eq!(outcome.output["truncated"], json!(false));
}

#[tokio::test]
async fn json_is_returned_as_it_arrived_rather_than_stripped() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"a":"<b>"}"#, "application/json"))
        .mount(&server)
        .await;

    let outcome = get(&tool(local()), &format!("{}/api", server.uri())).await;
    assert_eq!(outcome.output["content"], json!(r#"{"a":"<b>"}"#));
}

#[tokio::test]
async fn a_query_string_is_sent_even_though_the_resource_does_not_carry_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(wiremock::matchers::query_param("q", "kernel"))
        .respond_with(ResponseTemplate::new(200).set_body_string("found"))
        .mount(&server)
        .await;

    let outcome = get(&tool(local()), &format!("{}/search?q=kernel", server.uri())).await;
    assert_eq!(outcome.output["content"], json!("found"));
}

#[tokio::test]
async fn a_host_name_is_resolved_through_the_guarded_resolver_and_still_connects() {
    // Every other test here names an address literally, which never reaches the resolver at
    // all. This one goes through it: the addresses it returns carry no port, and the URL's
    // port has to be the one actually connected to.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/by-name"))
        .respond_with(ResponseTemplate::new(200).set_body_string("resolved"))
        .mount(&server)
        .await;

    let port = server.address().port();
    let outcome = get(&tool(local()), &format!("http://localhost:{port}/by-name")).await;
    assert_eq!(outcome.output["content"], json!("resolved"));
}

#[tokio::test]
async fn a_name_that_resolves_only_to_a_refused_address_is_not_connected_to() {
    // The other half of the same path: with local addresses off, `localhost` resolves to
    // exactly the addresses the boundary refuses, and the refusal names the range.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
        .mount(&server)
        .await;

    let settings = NetSettings {
        allow_http: true,
        ..NetSettings::default()
    };
    let port = server.address().port();
    let outcome = get(&tool(settings), &format!("http://localhost:{port}/x")).await;

    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("loopback"),
        "{:?}",
        outcome.output
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failing_status_is_something_the_model_sees_rather_than_a_failed_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("no such page"))
        .mount(&server)
        .await;

    let outcome = get(&tool(local()), &format!("{}/missing", server.uri())).await;
    assert!(outcome.is_error);
    assert_eq!(outcome.output["status"], json!(404));
    assert!(outcome.output["error"].as_str().unwrap().contains("404"));
}

// ---------------------------------------------------------------------------
// Bounds on what comes back
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_over_the_limit_is_cut_and_says_so() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .set_body_string("x".repeat(4096)),
        )
        .mount(&server)
        .await;

    let settings = NetSettings {
        max_bytes: Some(64),
        ..local()
    };
    let outcome = get(&tool(settings), &format!("{}/big", server.uri())).await;

    // Declared length over the limit is refused before the body is read at all.
    assert!(outcome.is_error);
    let reason = outcome.output["error"].as_str().unwrap();
    assert!(reason.contains("64-byte limit"), "{reason}");
}

#[tokio::test]
async fn a_body_whose_length_is_not_declared_is_bounded_as_it_arrives() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chunked"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .insert_header("transfer-encoding", "chunked")
                .set_body_string("y".repeat(4096)),
        )
        .mount(&server)
        .await;

    let settings = NetSettings {
        max_bytes: Some(64),
        ..local()
    };
    let outcome = get(&tool(settings), &format!("{}/chunked", server.uri())).await;

    assert!(!outcome.is_error, "{:?}", outcome.output);
    assert_eq!(outcome.output["truncated"], json!(true));
    assert!(outcome.output["content"].as_str().unwrap().len() <= 64);
}

#[tokio::test]
async fn a_binary_content_type_is_refused_instead_of_being_decoded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/image"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(vec![0x89, b'P', b'N', b'G']),
        )
        .mount(&server)
        .await;

    let outcome = get(&tool(local()), &format!("{}/image", server.uri())).await;
    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("image/png")
    );
    assert!(outcome.output.get("content").is_none());
}

#[tokio::test]
async fn a_charset_this_crate_does_not_decode_is_refused_rather_than_mangled() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/latin"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("caf\u{e9}", "text/html; charset=iso-8859-1"),
        )
        .mount(&server)
        .await;

    let outcome = get(&tool(local()), &format!("{}/latin", server.uri())).await;
    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("iso-8859-1")
    );
}

// ---------------------------------------------------------------------------
// Redirects: the case that is discovered mid-run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_redirect_is_followed_only_after_its_destination_is_authorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/from"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/to"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/to"))
        .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
        .mount(&server)
        .await;

    let records = Arc::new(Records::default());
    let outcome = tool(local())
        .invoke(
            json!({ "url": format!("{}/from", server.uri()) }),
            records.as_ref(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.output["content"], json!("arrived"));
    assert!(
        outcome.output["final_url"]
            .as_str()
            .unwrap()
            .ends_with("/to")
    );
    assert_eq!(outcome.output["redirects"].as_array().unwrap().len(), 1);

    // Both namespaces, for the hop that was not knowable when the call was authorized.
    let asked = records.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert!(asked[0].starts_with("host/127.0.0.1"), "{asked:?}");
    assert!(asked[1].ends_with("/to"), "{asked:?}");
}

#[tokio::test]
async fn a_redirect_the_authorizer_refuses_stops_the_call_and_is_not_fetched() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/from"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/to"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/to"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
        .mount(&server)
        .await;

    let refuses = Arc::new(Refuses::default());
    let error = tool(local())
        .invoke(
            json!({ "url": format!("{}/from", server.uri()) }),
            refuses.as_ref(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Permission);
    assert_eq!(refuses.asked.load(Ordering::SeqCst), 1);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "the refused hop was fetched anyway");
    assert_eq!(requests[0].url.path(), "/from");
}

#[tokio::test]
async fn a_redirect_to_a_denied_host_is_refused_without_being_fetched() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/away"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://elsewhere.test/x"),
        )
        .mount(&server)
        .await;

    let settings = NetSettings {
        deny_hosts: vec!["elsewhere.test".to_owned()],
        ..local()
    };
    let outcome = tool(settings)
        .invoke(
            json!({ "url": format!("{}/away", server.uri()) }),
            &Records::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert!(outcome.is_error);
    let reason = outcome.output["error"].as_str().unwrap();
    assert!(reason.contains("denied hosts"), "{reason}");
}

#[tokio::test]
async fn a_redirect_loop_stops_at_the_configured_bound() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;

    let settings = NetSettings {
        max_redirects: Some(2),
        ..local()
    };
    let outcome = tool(settings)
        .invoke(
            json!({ "url": format!("{}/loop", server.uri()) }),
            &Records::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert!(outcome.is_error);
    assert!(
        outcome.output["error"]
            .as_str()
            .unwrap()
            .contains("more than 2 redirects")
    );
    // The original plus exactly the two hops that were allowed.
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_redirect_the_client_would_have_followed_is_never_followed_by_the_client() {
    // The guarantee underneath every redirect test above: the HTTP client itself follows
    // nothing, so a hop that this crate does not decide about does not happen at all.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/end"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/end"))
        .respond_with(ResponseTemplate::new(200).set_body_string("end"))
        .mount(&server)
        .await;

    let settings = NetSettings {
        max_redirects: Some(0),
        ..local()
    };
    let outcome = tool(settings)
        .invoke(
            json!({ "url": format!("{}/hop", server.uri()) }),
            &Records::default(),
            &ExecutionContext::new(),
        )
        .await
        .unwrap();

    assert!(outcome.is_error);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
