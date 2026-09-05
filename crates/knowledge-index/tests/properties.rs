//! Property-based tests for strings an LLM client can supply.
//!
//! Invariants, not relevance: these prove that arbitrary query text, symbol
//! strings, ids, filter strings, limits, and markdown cannot panic, hang,
//! corrupt state, or produce unbounded output. Retrieval *quality* stays the
//! job of the eval harness (tests/eval.rs).
//!
//! Case counts are bounded (32 per property by default) to keep cargo test
//! fast; run heavier sweeps with PROPTEST_CASES=<n> cargo test (see
//! AGENTS.md, "Property-based and adversarial-input testing").

mod common;

use std::collections::HashSet;
use std::path::PathBuf;

use knowledge_core::{
    DocumentId, KnowledgeError, KnowledgeRetriever, PackageIdentity, SearchQuery, SourceKind,
    SymbolQuery,
};
use knowledge_index::markdown::{MAX_CHUNK_CHARS, MarkdownFile, chunk_markdown};
use proptest::prelude::*;

/// Structural separators (paragraph ends, list markers) are appended
/// without a bounds check, so a chunk may slightly exceed MAX_CHUNK_CHARS.
/// Same bound as the adversarial corpus test.
const CHUNK_SLACK: usize = 3;

/// Fragments an adversarial or confused LLM client might assemble strings
/// from: query syntax, path traversal, unicode tricks, package specs.
/// Duplicated verbatim in `crates/knowledge-mcp/tests/properties.rs` (no
/// shared test-support crate, by scope decision); keep both lists in sync.
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

/// Fragments that stress the markdown chunker's structure handling.
const MARKDOWN_FRAGMENTS: [&str; 19] = [
    "# ",
    "## ",
    "### ",
    "\n\n",
    "```rust\n",
    "```\n",
    "- ",
    "* ",
    "1. ",
    "[a](b)",
    "> ",
    "|x|",
    "`code`",
    "**bold**",
    "<!--",
    "-->",
    "text ",
    "engine ",
    "write_all ",
];

fn markdown_string() -> BoxedStrategy<String> {
    prop_oneof![
        3 => proptest::collection::vec(proptest::sample::select(MARKDOWN_FRAGMENTS.to_vec()), 0..48)
            .prop_map(|parts| parts.concat()),
        2 => hostile_string(),
        // A single unbroken word straddling the chunk size boundary: every
        // time this branch fires it exercises the oversized-event split.
        2 => proptest::string::string_regex("[a-z]{3800,4200}").expect("valid regex"),
        1 => Just(String::new()),
    ]
    .boxed()
}

/// A package identity for chunker inputs (ids derive from it; values here
/// only need to be stable, not real).
fn pkg() -> PackageIdentity {
    PackageIdentity {
        package_id: "properties#test-pkg@0.0.0".into(),
        name: "test-pkg".into(),
        version: "0.0.0".into(),
        source: None,
        manifest_path: PathBuf::from("/tmp/properties/Cargo.toml"),
    }
}

fn md_file() -> MarkdownFile {
    MarkdownFile {
        rel_path: "README.md".into(),
        abs_path: PathBuf::from("/tmp/properties/README.md"),
        kind: SourceKind::CrateReadme,
    }
}

/// True when the string, normalized the way filter_clause normalizes
/// package specs, names a package that exists in the fixture corpus.
fn names_a_package(spec: &str) -> bool {
    let normalized = spec.trim().to_lowercase();
    common::package_keys().contains(&normalized)
}

#[test]
fn shared_fixture_serves_queries() {
    // Smoke check for the OnceLock fixture: a plain search must work, the
    // fixture must expose known package keys, and its known-good sample
    // id must resolve to an intact document.
    let retriever = common::retriever();
    let hits = retriever
        .search(&SearchQuery::new("write_all"))
        .expect("fixture search");
    assert!(!hits.is_empty());
    let keys = common::package_keys();
    assert!(keys.iter().any(|k| k == "demo-core"), "keys: {keys:?}");
    let known = common::sample_id();
    let doc = retriever.get(&known).expect("sample document exists");
    assert_eq!(doc.id, known);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Arbitrary query text: never a panic, Ok or the structured
    /// empty-query error, bounded hits, ids that round-trip.
    #[test]
    fn search_never_panics_on_arbitrary_text(text in hostile_string(), limit in 0usize..64) {
        let retriever = common::retriever();
        let query = SearchQuery {
            limit,
            ..SearchQuery::new(text)
        };
        match retriever.search(&query) {
            Ok(hits) => {
                prop_assert!(hits.len() <= limit.max(1));
                for hit in &hits {
                    prop_assert_eq!(
                        DocumentId::from_raw(hit.id.as_str()),
                        Some(hit.id.clone()),
                        "hit id does not round-trip through from_raw"
                    );
                    // The snippet cap is enforced on bytes: tantivy 0.26.1
                    // applies set_max_num_chars to token byte offsets
                    // (tantivy-tokenizer-api's Token offsets are byte
                    // indices; the "characters" wording in tantivy's
                    // snippet doc comment is stale — a 2-byte Cyrillic
                    // corpus at SNIPPET_CHARS = 300 yields a 300-byte,
                    // 227-char snippet), and the truncate_at_word fallback
                    // truncates bytes too. 320 = the 300-byte cap plus
                    // token-boundary overshoot.
                    prop_assert!(
                        hit.snippet.len() <= 320,
                        "snippet is unbounded"
                    );
                }
            }
            Err(KnowledgeError::Engine(message)) if message == "empty query" => {}
            Err(other) => prop_assert!(false, "unexpected error: {other:?}"),
        }
    }

    /// Arbitrary symbol strings: never a panic, bounded results, valid ids.
    #[test]
    fn symbol_lookup_never_panics_on_arbitrary_text(
        text in hostile_string(),
        limit in 0usize..64,
    ) {
        let retriever = common::retriever();
        let query = SymbolQuery {
            limit,
            ..SymbolQuery::new(text)
        };
        match retriever.symbol_lookup(&query) {
            Ok(infos) => {
                prop_assert!(infos.len() <= limit.max(1));
                for info in &infos {
                    prop_assert!(
                        DocumentId::from_raw(info.id.as_str()).is_some(),
                        "result id is not a valid id"
                    );
                }
            }
            Err(other) => prop_assert!(false, "unexpected error: {other:?}"),
        }
    }

    /// Arbitrary id strings: malformed shapes are rejected, well-formed
    /// shapes resolve to an intact document or a typed not-found error.
    #[test]
    fn arbitrary_id_strings_validate_cleanly(text in hostile_string()) {
        match DocumentId::from_raw(text.as_str()) {
            None => {}
            Some(id) => {
                prop_assert_eq!(id.as_str().len(), 32);
                let retriever = common::retriever();
                match retriever.get(&id) {
                    Ok(doc) => prop_assert_eq!(doc.id, id, "got a different document"),
                    Err(KnowledgeError::DocumentNotFound(_)) => {}
                    Err(other) => prop_assert!(false, "unexpected error: {other:?}"),
                }
            }
        }
    }

    /// Well-formed but (almost surely) unknown ids: typed not-found errors,
    /// and any hit is an intact document carrying its own id.
    #[test]
    fn well_formed_unknown_ids_are_rejected_cleanly(
        hex in proptest::collection::vec(
            proptest::sample::select(vec![
                '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
            ]),
            32,
        ),
    ) {
        let raw: String = hex.into_iter().collect();
        let id = DocumentId::from_raw(&raw).expect("32 hex chars always parse");
        let retriever = common::retriever();
        match retriever.get(&id) {
            Ok(doc) => prop_assert_eq!(doc.id, id, "got a different document"),
            Err(KnowledgeError::DocumentNotFound(_)) => {}
            Err(other) => prop_assert!(false, "unexpected error: {other:?}"),
        }
    }

    /// Uppercase-hex ids must behave exactly like their lowercase form.
    #[test]
    fn uppercase_hex_ids_parse_identically(
        hex in proptest::collection::vec(
            proptest::sample::select(vec![
                '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
                'A', 'B', 'C', 'D', 'E', 'F',
            ]),
            32,
        ),
    ) {
        let raw: String = hex.into_iter().collect();
        let lower = raw.to_lowercase();
        prop_assert_eq!(DocumentId::from_raw(&raw), DocumentId::from_raw(&lower));
    }

    /// Garbage package filters must never widen results: a baseline query
    /// that demonstrably hits the fixture must return zero hits behind a
    /// garbage-only filter.
    #[test]
    fn garbage_package_filters_never_widen_results(
        filter in hostile_string().prop_filter("not a real package key", |s| {
            !names_a_package(s)
        }),
    ) {
        let retriever = common::retriever();
        let baseline = retriever
            .search(&SearchQuery::new("engine encode"))
            .expect("baseline search");
        prop_assert!(!baseline.is_empty(), "baseline query must hit the fixture");
        let query = SearchQuery {
            packages: vec![filter],
            ..SearchQuery::new("engine encode")
        };
        let hits = retriever.search(&query).expect("search with garbage filter");
        prop_assert!(hits.is_empty(), "garbage filter widened to {} hits", hits.len());
    }

    /// Garbage item-kind filters must never widen results.
    #[test]
    fn garbage_item_kind_filters_never_widen_results(
        filter in hostile_string().prop_filter("not a real item kind", |s| {
            let normalized = s.trim().to_lowercase();
            !common::item_kinds().contains(&normalized)
        }),
    ) {
        let retriever = common::retriever();
        let query = SearchQuery {
            item_kinds: vec![filter],
            ..SearchQuery::new("engine encode")
        };
        let hits = retriever.search(&query).expect("search with garbage filter");
        prop_assert!(hits.is_empty(), "garbage filter widened to {} hits", hits.len());
    }

    /// A valid package spec beside garbage keeps exactly its own semantics:
    /// adding garbage to the filter must not change the result set, and
    /// every hit must still come from the valid spec.
    #[test]
    fn mixed_valid_and_garbage_filters_keep_valid_semantics(
        valid in proptest::sample::select(common::package_keys().to_vec()),
        garbage in hostile_string().prop_filter("not a real package key", |s| {
            !names_a_package(s)
        }),
    ) {
        let retriever = common::retriever();
        let valid_only = retriever
            .search(&SearchQuery {
                packages: vec![valid.clone()],
                ..SearchQuery::new("engine encode")
            })
            .expect("search with valid filter");
        let with_garbage = retriever
            .search(&SearchQuery {
                packages: vec![valid.clone(), garbage],
                ..SearchQuery::new("engine encode")
            })
            .expect("search with mixed filter");
        let ids_of = |hits: &[knowledge_core::SearchHit]| {
            hits.iter()
                .map(|h| h.id.as_str().to_string())
                .collect::<Vec<_>>()
        };
        prop_assert_eq!(
            ids_of(&with_garbage),
            ids_of(&valid_only),
            "garbage beside a valid spec changed the result set"
        );
        let valid_name = valid.split('@').next().unwrap_or(valid.as_str());
        for hit in &valid_only {
            prop_assert_eq!(
                hit.package_name.as_str(),
                valid_name,
                "hit outside the valid filter set"
            );
        }
    }

    /// Limits bound result counts across the whole small range.
    #[test]
    fn limits_bound_result_counts(limit in 0usize..64) {
        let retriever = common::retriever();
        let hits = retriever
            .search(&SearchQuery {
                limit,
                ..SearchQuery::new("engine encode")
            })
            .expect("search");
        prop_assert!(hits.len() <= limit.max(1));
        let infos = retriever
            .symbol_lookup(&SymbolQuery {
                limit,
                ..SymbolQuery::new("Writer")
            })
            .expect("symbol lookup");
        prop_assert!(infos.len() <= limit.max(1));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Arbitrary markdown-ish input: no panic, deterministic output,
    /// bounded chunks, unique valid ids.
    #[test]
    fn chunk_markdown_survives_arbitrary_input(text in markdown_string()) {
        let docs = chunk_markdown(&pkg(), &md_file(), &text);
        let again = chunk_markdown(&pkg(), &md_file(), &text);
        prop_assert!(docs == again, "chunking is not deterministic");

        let mut ids = HashSet::new();
        for doc in &docs {
            prop_assert!(
                doc.text.len() <= MAX_CHUNK_CHARS + CHUNK_SLACK,
                "chunk is {} bytes, exceeding MAX_CHUNK_CHARS ({}) + {}",
                doc.text.len(),
                MAX_CHUNK_CHARS,
                CHUNK_SLACK
            );
            prop_assert!(ids.insert(doc.id.clone()), "duplicate id: {}", doc.id);
            prop_assert!(
                DocumentId::from_raw(doc.id.as_str()).is_some(),
                "id is not valid hex"
            );
        }
    }
}
