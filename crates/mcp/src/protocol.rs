//! The wire shapes, and the point at which a server's output stops being trusted.
//!
//! Everything in this module reads bytes that came from a program the kernel started but
//! does not control. A tool server writes its own name, its own descriptions, its own JSON
//! Schemas and its own results, and every one of those reaches a model — which is to say
//! every one of them is attacker-influenced input in any deployment where the server is not
//! part of the trusted computing base.
//!
//! So the parse is deliberately narrow, and it refuses rather than repairs:
//!
//! * A tool name must be a plain name — ASCII letters, digits, `_` and `-`. A server cannot offer `../..`, a name
//!   with a `.` in it (which is how the namespace this crate builds is punctuated), whitespace
//!   that renders as one name and matches another, or a control character that erases what a
//!   human is shown when the call is put up for approval.
//! * An input schema must be a JSON object describing an object. A schema that is a bare
//!   `true`, a string, or an array is not something a model provider can be handed.
//! * Descriptions are bounded and stripped of control characters, because they are prompt
//!   text: the description is copied verbatim into the model's tool list.
//! * A result is bounded, and binary content is described rather than carried. A server that
//!   returns a ten-megabyte base64 image would otherwise turn one tool call into a context
//!   window nobody can afford, and the model cannot see the image anyway.
//!
//! Whether a *refusal* drops the tool or fails the listing is decided in [`crate::catalog`],
//! not here. This module's job is to have exactly one place where a byte from a server
//! becomes a value the rest of the kernel will act on.

use aik_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The JSON-RPC version every frame carries.
pub(crate) const JSONRPC_VERSION: &str = "2.0";

/// The MCP revision this client speaks unless a deployment names another.
///
/// A server that answers with a different revision is checked against
/// [`SUPPORTED_PROTOCOL_VERSIONS`] rather than against this constant: the handshake exists
/// so that two implementations can agree on an older revision they both know.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// The MCP revisions this client can work with.
///
/// All three describe `tools/list` and `tools/call` in the shape this crate parses. A server
/// reporting anything else ends the session at startup instead of being talked to in a
/// dialect neither side has agreed on.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The longest tool description kept, in bytes.
pub(crate) const MAX_DESCRIPTION_BYTES: usize = 8 * 1024;

/// The longest tool name accepted from a server.
pub(crate) const MAX_TOOL_NAME_BYTES: usize = 128;

/// The JSON-RPC error code for a method the receiver does not implement.
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;

/// A request id. Sequential and client-generated, so a server cannot choose one.
pub(crate) type RequestId = u64;

/// One frame read from a server.
///
/// Classified by [`classify`] rather than by `#[serde(untagged)]`, because untagged
/// deserialisation ignores unknown fields: a server-initiated request (`id` plus `method`)
/// would satisfy the response variant with no result and no error, and be silently dropped
/// instead of refused. Which frame this is decides whether the client answers a server or
/// wakes a caller, so it is decided explicitly.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Incoming {
    /// An answer to something this client asked.
    Response {
        /// The id of the request being answered.
        id: RequestId,
        /// The result, when the call succeeded.
        result: Option<Value>,
        /// The failure, when it did not.
        error: Option<RpcError>,
    },
    /// Something the server is asking *this* client to do.
    ///
    /// Recognised only so that it can be refused by id — see [`crate::session`]. This client
    /// advertises no capabilities, so there is no such request it answers.
    Request {
        /// The id to answer.
        id: RequestId,
        /// What is being asked.
        method: String,
    },
    /// A one-way message. Ignored.
    Notification {
        /// What is being announced.
        method: String,
    },
}

/// Decides what a parsed frame is.
///
/// An id this client cannot have issued — a string, a negative number, one past the counter
/// — is not matched against anything pending; it is refused as unroutable, so a server
/// cannot answer a request nobody made or provoke a lookup with a value of its choosing.
pub(crate) fn classify(frame: &Value) -> Result<Incoming> {
    let method = frame.get("method").and_then(Value::as_str);
    let id = match frame.get("id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            Error::other("the server used a request id this client cannot have issued")
        })?),
    };

    match (id, method) {
        (Some(id), Some(method)) => Ok(Incoming::Request {
            id,
            method: sanitize(method, 128),
        }),
        (Some(id), None) => {
            let error = match frame.get("error") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    serde_json::from_value::<RpcError>(value.clone())
                        .map_err(|_| Error::other("the server sent a malformed JSON-RPC error"))?,
                ),
            };
            let result = frame
                .get("result")
                .cloned()
                .filter(|value| !value.is_null());
            if result.is_none() && error.is_none() {
                return Err(Error::other(
                    "the server answered with neither a result nor an error",
                ));
            }
            Ok(Incoming::Response { id, result, error })
        }
        (None, Some(method)) => Ok(Incoming::Notification {
            method: sanitize(method, 128),
        }),
        (None, None) => Err(Error::other(
            "the server sent a frame that is neither a response, a request nor a notification",
        )),
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct RpcError {
    /// The machine-readable code.
    pub(crate) code: i64,
    /// The human-readable message.
    pub(crate) message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

/// Builds a request frame.
pub(crate) fn request(id: RequestId, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Builds a notification frame.
pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": method,
        "params": params,
    })
}

/// Builds the refusal sent for any request a server makes of this client.
pub(crate) fn method_not_found(id: RequestId, method: &str) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": METHOD_NOT_FOUND,
            "message": format!(
                "this client advertises no capabilities, so `{method}` is not available to a server"
            ),
        },
    })
}

/// The `initialize` parameters.
///
/// The advertised capability set is empty on purpose, and that is a security property rather
/// than an omission. `sampling` would let a server ask this kernel's model to generate text —
/// turning a tool call into a model call the deployment pays for and did not ask for, with a
/// prompt the server wrote. `roots` would tell a server where on the host this deployment
/// keeps its files. Neither is needed to list and call tools, so neither is offered, and
/// [`method_not_found`] is what a server that asks anyway receives.
pub(crate) fn initialize_params(protocol_version: &str, client_name: &str) -> Value {
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {},
        "clientInfo": {
            "name": client_name,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// What a server said about itself in its `initialize` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    /// The revision the server agreed to speak.
    pub protocol_version: String,
    /// The server's self-reported name, bounded and stripped.
    pub server_name: Option<String>,
    /// The server's self-reported version, bounded and stripped.
    pub server_version: Option<String>,
    /// Whether the server declared a `tools` capability.
    pub serves_tools: bool,
}

/// Reads an `initialize` result, or explains why the session cannot continue.
pub(crate) fn parse_hello(result: &Value) -> Result<ServerHello> {
    let protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::other("the server's `initialize` result names no `protocolVersion`")
        })?;

    if !SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol_version) {
        return Err(Error::Unsupported(format!(
            "the server speaks MCP revision `{}`, and this client speaks {}",
            sanitize(protocol_version, 64),
            SUPPORTED_PROTOCOL_VERSIONS.join(", ")
        )));
    }

    let info = result.get("serverInfo");
    Ok(ServerHello {
        protocol_version: protocol_version.to_owned(),
        server_name: info
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(|name| sanitize(name, 128)),
        server_version: info
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str)
            .map(|version| sanitize(version, 64)),
        serves_tools: result
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("tools"))
            .is_some(),
    })
}

/// One tool a server offers, after the untrusted shape has been checked.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteToolDefinition {
    /// The name the server calls it, as it must be sent back in `tools/call`.
    pub remote_name: String,
    /// What it does, bounded and stripped.
    pub description: String,
    /// The JSON Schema of its input object.
    pub input_schema: Value,
    /// The JSON Schema of its output, when the server declared one.
    pub output_schema: Option<Value>,
}

/// Reads a `tools/list` result.
///
/// Returns the definitions and the pagination cursor the server offered, if any.
pub(crate) fn parse_tool_list(
    result: &Value,
) -> Result<(Vec<RemoteToolDefinition>, Option<String>)> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::other("the server's `tools/list` result carries no `tools` array"))?;

    let definitions = tools
        .iter()
        .map(parse_tool_definition)
        .collect::<Result<Vec<_>>>()?;

    let cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(|cursor| sanitize(cursor, 1024));

    Ok((definitions, cursor))
}

/// Reads one entry of a `tools/list` result.
fn parse_tool_definition(value: &Value) -> Result<RemoteToolDefinition> {
    let remote_name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::other("a tool in `tools/list` has no `name`"))?;
    validate_remote_name(remote_name)?;

    let input_schema = value
        .get("inputSchema")
        .ok_or_else(|| Error::other(format!("tool `{remote_name}` declares no `inputSchema`")))?;
    let input_schema = validate_schema(remote_name, "inputSchema", input_schema)?;

    let output_schema = match value.get("outputSchema") {
        None | Some(Value::Null) => None,
        Some(schema) => Some(validate_schema(remote_name, "outputSchema", schema)?),
    };

    // `title` is the human-facing label and `description` the model-facing one. Both are
    // prompt text; the description is what a model reasons about, so it is what is kept.
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(|text| sanitize(text, MAX_DESCRIPTION_BYTES))
        .unwrap_or_default();

    Ok(RemoteToolDefinition {
        remote_name: remote_name.to_owned(),
        description,
        input_schema,
        output_schema,
    })
}

/// The characters a server's tool name may contain.
///
/// `.` is excluded deliberately, even though MCP permits it: this crate builds a namespace
/// punctuated by `.` (`mcp.<server>.<tool>`), and a remote name containing one could produce
/// a kernel-side name that reads as belonging to a different server.
fn is_name_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

/// Accepts a server's tool name, or explains why it is not usable as one.
pub(crate) fn validate_remote_name(raw: &str) -> Result<()> {
    let refuse = |why: &str| {
        Err(Error::InvalidArgument(format!(
            "`{}` is not a usable tool name: {why}",
            sanitize(raw, 64)
        )))
    };

    if raw.is_empty() {
        return refuse("it is empty");
    }
    if raw.len() > MAX_TOOL_NAME_BYTES {
        return refuse("it is longer than this client accepts");
    }
    if !raw.bytes().all(is_name_char) {
        return refuse(
            "only ASCII letters, digits, `_` and `-` are allowed, so that a name cannot \
             punctuate the namespace it is placed in or render as a different name than it is",
        );
    }
    Ok(())
}

/// Accepts a JSON Schema that describes an object, or explains why it cannot be used.
///
/// Only the outermost shape is checked. Validating a schema in full is a different job, and
/// one this crate deliberately does not do: the model provider and the server both read the
/// schema, and a schema this client understood differently from either of them would be a
/// third opinion nobody asked for. What is checked is the part the kernel itself relies on —
/// that there is an object with properties to show a model — because a `true` or a string
/// there becomes a malformed tool definition sent to a provider.
fn validate_schema(tool: &str, field: &str, schema: &Value) -> Result<Value> {
    let Some(object) = schema.as_object() else {
        return Err(Error::other(format!(
            "tool `{tool}`'s `{field}` is not a JSON object"
        )));
    };

    match object.get("type").and_then(Value::as_str) {
        Some("object") => Ok(schema.clone()),
        Some(other) => Err(Error::other(format!(
            "tool `{tool}`'s `{field}` describes a `{}`, and a tool's arguments are an object",
            sanitize(other, 32)
        ))),
        // A schema with no `type` is not refused: `{"$ref": ...}` and a bare `{}` are both
        // legitimate ways to say "an object with anything in it", and refusing them would
        // reject working servers over a formality. It is normalised instead, so whatever is
        // handed to a provider always says what it is.
        None => {
            let mut normalised = object.clone();
            normalised.insert("type".into(), json!("object"));
            Ok(Value::Object(normalised))
        }
    }
}

/// What a `tools/call` produced, after bounding.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteToolResult {
    /// The value handed back to the model.
    pub output: Value,
    /// Whether the server reported this as a failure the model should see.
    pub is_error: bool,
}

/// Reads a `tools/call` result, keeping at most `max_bytes` of text.
///
/// A server that reports `isError` has failed in a way the model should reason about, which
/// is [`ToolOutcome::is_error`](aik_api::tool::ToolOutcome::is_error) and not an `Err`. A
/// server that fails at the JSON-RPC level has not run the tool at all, and that is handled
/// by the caller.
pub(crate) fn parse_tool_result(result: &Value, max_bytes: usize) -> RemoteToolResult {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut output = Map::new();

    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let (text, blocks) = flatten_content(content, max_bytes);
        output.insert("text".into(), json!(text));
        if !blocks.is_empty() {
            output.insert("content".into(), Value::Array(blocks));
        }
    }

    // Carried through unchanged when the server declared an output schema, because that is
    // the shape the model was told to expect. It is bounded by re-serialising and measuring:
    // a structured result is still a result a server chose the size of.
    if let Some(structured) = result.get("structuredContent")
        && !structured.is_null()
    {
        match bounded_json(structured, max_bytes) {
            Some(value) => {
                output.insert("structuredContent".into(), value);
            }
            None => {
                output.insert(
                    "structuredContent".into(),
                    json!({
                        "omitted": "the server's structured result was larger than this \
                                    deployment accepts"
                    }),
                );
            }
        }
    }

    if output.is_empty() {
        output.insert("text".into(), json!(""));
    }

    RemoteToolResult {
        output: Value::Object(output),
        is_error,
    }
}

/// Splits content blocks into the text a model reads and a description of everything else.
///
/// Binary blocks are described, never carried. A base64 image is bytes a language model
/// cannot see through this path, and carrying one would spend the whole result budget on
/// something nobody reads.
fn flatten_content(content: &[Value], max_bytes: usize) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut truncated = false;
    let mut others = Vec::new();

    for block in content {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "text" => {
                let Some(chunk) = block.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !text.is_empty() {
                    text.push('\n');
                }
                let remaining = max_bytes.saturating_sub(text.len());
                if chunk.len() > remaining {
                    text.push_str(&sanitize(chunk, remaining));
                    truncated = true;
                    break;
                }
                text.push_str(&sanitize(chunk, remaining));
            }
            other => {
                let bytes = block
                    .get("data")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
                others.push(json!({
                    "type": sanitize(other, 32),
                    "encoded_bytes": bytes,
                    "omitted": "non-text content is described rather than carried",
                }));
            }
        }
    }

    if truncated {
        text.push_str("\n[truncated: the server returned more than this deployment accepts]");
    }
    (text, others)
}

/// Returns `value` if it serialises to at most `max_bytes`, otherwise nothing.
fn bounded_json(value: &Value, max_bytes: usize) -> Option<Value> {
    let encoded = serde_json::to_string(value).ok()?;
    (encoded.len() <= max_bytes).then(|| value.clone())
}

/// Bounds a server-supplied string and removes what would misrepresent it when displayed.
///
/// Control characters go because this text is shown to a human deciding whether to approve a
/// call, and a carriage return or an ANSI escape can make a line say something other than
/// what it is. Tabs and newlines are kept: a tool description is prose, and flattening it
/// would mangle every server that formats one.
///
/// Truncation is on a character boundary, so the result is always valid UTF-8.
pub(crate) fn sanitize(raw: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    for character in raw.chars() {
        let keep = match character {
            '\n' | '\t' => character,
            control if control.is_control() => ' ',
            other => other,
        };
        if out.len() + keep.len_utf8() > max_bytes {
            break;
        }
        out.push(keep);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aik_core::ErrorKind;

    #[test]
    fn a_plain_remote_name_is_accepted() {
        for name in ["read_file", "list-dirs", "x", "A1"] {
            validate_remote_name(name).unwrap_or_else(|error| panic!("{name}: {error}"));
        }
    }

    #[test]
    fn a_name_that_could_punctuate_the_namespace_is_refused() {
        // The failure this rules out is a server naming a tool `github.read` and producing a
        // kernel-side `mcp.files.github.read`, which reads as a tool of a server called
        // `files.github`.
        for raw in [
            "",
            "a.b",
            "../escape",
            "with space",
            "new\nline",
            "esc\u{1b}[0m",
            "unicode\u{2024}dot",
        ] {
            let error = validate_remote_name(raw).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{raw:?}");
        }
    }

    #[test]
    fn a_name_longer_than_the_cap_is_refused() {
        let long = "a".repeat(MAX_TOOL_NAME_BYTES + 1);
        assert!(validate_remote_name(&long).is_err());
    }

    #[test]
    fn a_schema_that_is_not_an_object_is_refused() {
        for schema in [json!(true), json!("object"), json!([]), json!(7)] {
            assert!(
                validate_schema("t", "inputSchema", &schema).is_err(),
                "{schema}"
            );
        }
    }

    #[test]
    fn a_schema_describing_something_other_than_an_object_is_refused() {
        let schema = json!({ "type": "string" });
        assert!(validate_schema("t", "inputSchema", &schema).is_err());
    }

    #[test]
    fn a_schema_with_no_type_is_normalised_rather_than_refused() {
        let schema = json!({ "properties": { "path": { "type": "string" } } });
        let checked = validate_schema("t", "inputSchema", &schema).unwrap();
        assert_eq!(checked["type"], json!("object"));
        assert_eq!(checked["properties"]["path"]["type"], json!("string"));
    }

    #[test]
    fn a_tool_whose_name_is_unusable_fails_the_whole_listing() {
        let result = json!({ "tools": [
            { "name": "fine", "inputSchema": { "type": "object" } },
            { "name": "not fine", "inputSchema": { "type": "object" } },
        ] });
        assert!(parse_tool_list(&result).is_err());
    }

    #[test]
    fn a_description_is_bounded_and_stripped() {
        let raw = format!(
            "hello\u{1b}[31mworld\r\n{}",
            "x".repeat(MAX_DESCRIPTION_BYTES)
        );
        let result = json!({ "tools": [
            { "name": "t", "description": raw, "inputSchema": { "type": "object" } },
        ] });
        let (tools, cursor) = parse_tool_list(&result).unwrap();
        assert!(cursor.is_none());
        assert!(tools[0].description.len() <= MAX_DESCRIPTION_BYTES);
        assert!(!tools[0].description.contains('\u{1b}'));
        assert!(!tools[0].description.contains('\r'));
        assert!(
            tools[0].description.contains('\n'),
            "prose newlines survive"
        );
    }

    #[test]
    fn an_unsupported_protocol_revision_ends_the_session() {
        let hello = json!({ "protocolVersion": "1999-01-01" });
        let error = parse_hello(&hello).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn a_supported_revision_is_read_with_what_the_server_said_about_itself() {
        let hello = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "files\u{7}", "version": "1.2.3" },
        });
        let parsed = parse_hello(&hello).unwrap();
        assert_eq!(parsed.protocol_version, "2024-11-05");
        assert_eq!(parsed.server_name.as_deref(), Some("files "));
        assert!(parsed.serves_tools);
    }

    #[test]
    fn text_content_is_joined_and_binary_content_is_described() {
        let result = json!({ "content": [
            { "type": "text", "text": "first" },
            { "type": "image", "data": "AAAA", "mimeType": "image/png" },
            { "type": "text", "text": "second" },
        ] });
        let parsed = parse_tool_result(&result, 1024);
        assert_eq!(parsed.output["text"], json!("first\nsecond"));
        assert_eq!(parsed.output["content"][0]["type"], json!("image"));
        assert_eq!(parsed.output["content"][0]["encoded_bytes"], json!(4));
        assert!(!parsed.is_error);
    }

    #[test]
    fn an_oversized_result_is_truncated_and_says_so() {
        let result = json!({ "content": [{ "type": "text", "text": "x".repeat(4096) }] });
        let parsed = parse_tool_result(&result, 64);
        let text = parsed.output["text"].as_str().unwrap();
        assert!(text.contains("truncated"), "{text}");
    }

    #[test]
    fn a_server_reported_failure_is_an_outcome_rather_than_an_error() {
        let result =
            json!({ "isError": true, "content": [{ "type": "text", "text": "no such file" }] });
        let parsed = parse_tool_result(&result, 1024);
        assert!(parsed.is_error);
        assert_eq!(parsed.output["text"], json!("no such file"));
    }

    #[test]
    fn an_oversized_structured_result_is_replaced_rather_than_carried() {
        let result = json!({ "structuredContent": { "blob": "x".repeat(4096) } });
        let parsed = parse_tool_result(&result, 64);
        assert!(parsed.output["structuredContent"]["omitted"].is_string());
    }

    #[test]
    fn a_server_request_carries_an_id_and_a_method_and_is_never_read_as_an_answer() {
        // The failure this rules out is a `sampling/createMessage` request being classified
        // as a response with no result, silently dropped instead of refused.
        let frame = classify(&json!({
            "jsonrpc": "2.0", "id": 9, "method": "sampling/createMessage"
        }))
        .unwrap();
        assert!(matches!(frame, Incoming::Request { id: 9, .. }));
    }

    #[test]
    fn a_response_carries_an_id_and_no_method() {
        let frame =
            classify(&json!({ "jsonrpc": "2.0", "id": 3, "result": { "ok": true } })).unwrap();
        match frame {
            Incoming::Response { id, result, error } => {
                assert_eq!(id, 3);
                assert!(result.is_some());
                assert!(error.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_notification_carries_a_method_and_no_id() {
        let frame =
            classify(&json!({ "jsonrpc": "2.0", "method": "notifications/message" })).unwrap();
        assert!(matches!(frame, Incoming::Notification { .. }));
    }

    #[test]
    fn an_id_this_client_could_not_have_issued_is_refused() {
        for id in [json!("1"), json!(-1), json!(1.5)] {
            let frame = json!({ "jsonrpc": "2.0", "id": id, "result": {} });
            assert!(classify(&frame).is_err(), "{id}");
        }
    }

    #[test]
    fn an_answer_with_neither_a_result_nor_an_error_is_refused() {
        assert!(classify(&json!({ "jsonrpc": "2.0", "id": 1 })).is_err());
    }
}
