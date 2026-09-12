//! Property tests over MCP tool parameters through the real in-memory
//! transport: arbitrary strings as query/symbol/id/filter arguments and
//! arbitrary limits must never panic and must produce either friendly
//! error results or bounded successes.
//!
//! Case counts are low (16 per property) because every case is a full
//! duplex round trip; heavier sweeps with PROPTEST_CASES=<n> (see
//! AGENTS.md, "Property-based and adversarial-input testing").

mod common;

use proptest::prelude::*;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

/// Fragments an adversarial or confused LLM client might assemble tool
/// arguments from: query syntax, path traversal, unicode, package specs.
/// Duplicated verbatim from `crates/knowledge-index/tests/properties.rs`
/// (no shared test-support crate); keep both lists in sync.
const HOSTILE_FRAGMENTS: [&str; 12] = [
    "*", "\"", "::", "\u{0}", "\u{200b}", " AND ", " NOT ", " OR ", "@", "..", "/", "base64",
];

/// Arbitrary hostile strings: bounded size, good shrinking.
fn hostile_string() -> BoxedStrategy<String> {
    prop_oneof![
        3 => proptest::string::string_regex("[ -~]{0,32}").expect("valid regex"),
        2 => proptest::string::string_regex("[ -~]{1,12}( [ -~]{1,12})*").expect("valid regex"),
        3 => proptest::collection::vec(proptest::sample::select(HOSTILE_FRAGMENTS.to_vec()), 0..6)
            .prop_map(|parts| parts.concat()),
        2 => proptest::collection::vec(proptest::char::any(), 0..10)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ]
    .boxed()
}

/// Extracts the JSON payload from a successful tool result.
fn payload_of(result: &CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|c| match c {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .expect("text content");
    serde_json::from_str(text).expect("result payload is JSON")
}

#[test]
fn knowledge_search_handles_arbitrary_tool_arguments() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let client = rt.block_on(common::serve(common::fixture_retriever()));
    proptest! {
        ProptestConfig::with_cases(16),
        |(query in hostile_string(),
          package in hostile_string(),
          item_kind in hostile_string(),
          source_kind in hostile_string(),
          limit in proptest::option::of(any::<u64>()))| {
            let arguments = json!({
                "query": query,
                "packages": [package],
                "item_kinds": [item_kind],
                "source_kinds": [source_kind],
                "limit": limit,
            });
            let result = rt
                .block_on(client.call_tool(common::tool_params("knowledge_search", arguments)))
                .expect("transport call must not fail");
            prop_assert!(!result.content.is_empty(), "empty content");
            if result.is_error.unwrap_or(false) {
                // Friendly rejection (e.g. an unknown source kind).
            } else {
                let payload = payload_of(&result);
                let results = payload
                    .get("results")
                    .and_then(Value::as_array)
                    .expect("results array");
                prop_assert!(
                    results.len() <= 50,
                    "{} results exceed the server clamp",
                    results.len()
                );
            }
        }
    }
    rt.block_on(client.cancel()).expect("cancel client");
}

#[test]
fn symbol_lookup_handles_arbitrary_tool_arguments() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let client = rt.block_on(common::serve(common::fixture_retriever()));
    proptest! {
        ProptestConfig::with_cases(16),
        |(symbol in hostile_string(),
          package in hostile_string(),
          limit in proptest::option::of(any::<u64>()))| {
            let arguments = json!({
                "symbol": symbol,
                "packages": [package],
                "limit": limit,
            });
            let result = rt
                .block_on(client.call_tool(common::tool_params("symbol_lookup", arguments)))
                .expect("transport call must not fail");
            prop_assert!(!result.content.is_empty(), "empty content");
            if result.is_error.unwrap_or(false) {
                // Friendly rejection.
            } else {
                let payload = payload_of(&result);
                let results = payload
                    .get("results")
                    .and_then(Value::as_array)
                    .expect("results array");
                prop_assert!(
                    results.len() <= 50,
                    "{} results exceed the server clamp",
                    results.len()
                );
            }
        }
    }
    rt.block_on(client.cancel()).expect("cancel client");
}

#[test]
fn doc_read_handles_arbitrary_tool_arguments() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let client = rt.block_on(common::serve(common::fixture_retriever()));
    proptest! {
        ProptestConfig::with_cases(16),
        |(id in hostile_string())| {
            let arguments = json!({ "id": id });
            let result = rt
                .block_on(client.call_tool(common::tool_params("doc_read", arguments)))
                .expect("transport call must not fail");
            prop_assert!(!result.content.is_empty(), "empty content");
            if result.is_error.unwrap_or(false) {
                // Friendly rejection for malformed or unknown ids.
            } else {
                let payload = payload_of(&result);
                prop_assert_eq!(
                    payload.get("id").and_then(Value::as_str).map(str::len),
                    Some(32),
                    "doc_read success must carry a well-formed id"
                );
            }
        }
    }
    rt.block_on(client.cancel()).expect("cancel client");
}
