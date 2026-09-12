//! The MCP-layer adversarial corpus: hostile strings as tool arguments
//! through the real in-memory duplex transport, on every cargo test.
//!
//! Tool-level failures must come back as friendly error results, successes
//! must be bounded JSON, and nothing may panic (a panicked server task
//! tears down the transport, which surfaces as a failed call, not a hang).
//! The canonical full payload list lives in the knowledge-index tests;
//! this focused list covers what the transport actually sees.

mod common;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

/// Hostile payloads for tool arguments.
fn payloads() -> Vec<(&'static str, String)> {
    vec![
        ("empty", String::new()),
        ("whitespace only", "   \t \n\r\n  ".into()),
        ("nul bytes", "\u{0}n\u{0}u\u{0}l\u{0}".into()),
        (
            "control characters",
            "\u{1}\u{7}\u{8}\u{b}\u{c}\u{e}\u{1f}".into(),
        ),
        ("cyrillic homoglyph", "d\u{435}mo-cor\u{435}".into()),
        (
            "zero-width characters",
            "demo\u{200b}\u{200c}\u{200d}-core".into(),
        ),
        ("path traversal", "../../../etc/passwd".into()),
        ("asterisk bomb", "* * ** *** *?*".into()),
        ("boolean operators", "AND OR NOT".into()),
        (
            "query syntax",
            "\"term\" +must -mustnot field:value~2 *:*".into(),
        ),
        ("deep path", "a::b::c::d::e::f::g::h".into()),
        ("json metacharacters", "{\"a\": [1, 2], \"b\": null}".into()),
        ("empty package spec", String::new()),
        ("double at", "base64@@0.21.7".into()),
        ("one megabyte word", "a".repeat(1_048_576)),
        ("fifty thousand newlines", "\n".repeat(50_000)),
    ]
}

#[test]
fn every_payload_through_every_tool_is_handled_cleanly() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let client = rt.block_on(common::serve(common::fixture_retriever()));

    for (name, payload) in payloads() {
        let cases: Vec<(&str, Value)> = vec![
            ("knowledge_search", json!({"query": payload})),
            (
                "knowledge_search",
                json!({"query": "engine encode", "packages": [payload]}),
            ),
            (
                "knowledge_search",
                json!({"query": "engine encode", "item_kinds": [payload]}),
            ),
            (
                "knowledge_search",
                json!({"query": "engine encode", "source_kinds": [payload]}),
            ),
            ("symbol_lookup", json!({"symbol": payload})),
            (
                "symbol_lookup",
                json!({"symbol": "Writer::write_all", "packages": [payload]}),
            ),
            ("doc_read", json!({"id": payload})),
        ];
        for (tool, arguments) in cases {
            let result = rt
                .block_on(client.call_tool(common::tool_params(tool, arguments)))
                .expect("transport call must not fail");
            check_result(name, tool, &result);
        }
    }

    rt.block_on(client.cancel()).expect("cancel client");
}

/// Limits the parameter schema cannot represent are rejected at the
/// parameter layer; huge in-range values clamp to the server bound.
#[test]
fn hostile_limits_are_clamped_or_rejected() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let client = rt.block_on(common::serve(common::fixture_retriever()));

    let limits: Vec<Value> = vec![
        json!(0),
        json!(1),
        json!(50),
        json!(1_000_000),
        json!(u64::MAX),
        json!(-1),
        json!(1.5),
        json!("8"),
        json!(null),
        json!([8]),
    ];
    for limit in limits {
        let result = rt.block_on(client.call_tool(common::tool_params(
            "knowledge_search",
            json!({"query": "engine encode", "limit": limit}),
        )));
        match result {
            // Type-invalid limits are rejected at the protocol layer.
            Err(_) => {}
            Ok(result) => check_result("hostile limit", "knowledge_search", &result),
        }
    }

    rt.block_on(client.cancel()).expect("cancel client");
}

/// Tool-level failures are friendly error results; successes are bounded
/// JSON whose shape matches the tool contract.
fn check_result(payload_name: &str, tool: &str, result: &CallToolResult) {
    assert!(
        !result.content.is_empty(),
        "{payload_name} / {tool}: empty content"
    );
    if result.is_error.unwrap_or(false) {
        return; // friendly rejection with a message
    }
    let text = result
        .content
        .first()
        .and_then(|c| match c {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .expect("text content");
    let payload: Value = serde_json::from_str(text).expect("result payload is JSON");
    match tool {
        "doc_read" => {
            let id = payload
                .get("id")
                .and_then(Value::as_str)
                .expect("doc_read payload carries the id");
            assert_eq!(id.len(), 32, "{payload_name} / {tool}: bad id");
        }
        _ => {
            let results = payload
                .get("results")
                .and_then(Value::as_array)
                .expect("results array");
            assert!(
                results.len() <= 50,
                "{payload_name} / {tool}: {} results exceed the server clamp",
                results.len()
            );
            for hit in results {
                assert!(
                    hit.get("id").and_then(Value::as_str).is_some(),
                    "{payload_name} / {tool}: hit without an id"
                );
            }
        }
    }
}
