//! The committed adversarial corpus: named hostile payloads, run through
//! every entry point an LLM client can reach, on every `cargo test`.
//!
//! Deterministic and fast — no randomness here (randomized coverage lives in
//! `properties.rs`). This file pins the concrete threat model from issue #5:
//! control characters, NUL bytes, unicode lookalikes, path traversal, query
//! metacharacters, markdown fence bombs, and enormous strings.
//!
//! To add a payload, append a `("name", ...)` entry in [payloads]; every
//! test in this file picks it up automatically.

mod common;

use std::collections::HashSet;
use std::path::PathBuf;

use knowledge_core::{
    DocumentId, KnowledgeError, KnowledgeRetriever, PackageIdentity, SearchQuery, SourceKind,
    SymbolQuery,
};
use knowledge_index::markdown::{MAX_CHUNK_CHARS, MarkdownFile, chunk_markdown};

/// Structural separators ("\n\n", "\n- ") are appended without a bounds
/// check, so a chunk may slightly exceed [MAX_CHUNK_CHARS]. This is the
/// documented worst-case slack; anything larger is a chunker bug.
const CHUNK_SLACK: usize = 3;

/// The named adversarial payloads. Structural "bombs" (fences, brackets,
/// emphasis) are capped at a few kilobytes so the corpus stays fast; the
/// ~1MB entries are plain text, where linear parsing is guaranteed.
fn payloads() -> Vec<(&'static str, String)> {
    vec![
        ("empty", String::new()),
        ("whitespace only", "   \t \n\r\n  ".into()),
        ("nul bytes", "\0n\0u\0l\0".into()),
        (
            "control characters",
            "\u{1}\u{7}\u{8}\u{b}\u{c}\u{e}\u{1f}".into(),
        ),
        ("cyrillic homoglyph", "d\u{0435}mo-cor\u{0435}".into()),
        (
            "zero-width characters",
            "demo\u{200b}\u{200c}\u{200d}-core".into(),
        ),
        ("combining marks", "write_a\u{301}ll".into()),
        ("right-to-left override", "\u{202e}base64".into()),
        ("fullwidth lookalike", "\u{ff44}emo-core".into()),
        ("cjk text", "\u{4e2d}\u{6587}\u{8bf4}\u{660e}".into()),
        ("path traversal", "../../../etc/passwd".into()),
        ("absolute path", "/etc/passwd".into()),
        ("windows path", "..\\..\\windows\\system32".into()),
        ("lone quote", "\"".into()),
        ("quote runs", "\"\"\"\"\"".into()),
        ("colon runs", "::::".into()),
        ("deep path", "a::b::c::d::e::f::g::h".into()),
        ("asterisk bomb", "* * ** *** *?*".into()),
        ("boolean operators", "AND OR NOT".into()),
        ("plus minus caret", "+must -mustnot ^boost".into()),
        ("field colon syntax", "title:write_all".into()),
        ("range syntax", "title:[a TO z]".into()),
        ("fuzzy tilde", "write_all~2".into()),
        ("star colon", "*:*".into()),
        ("json metacharacters", "{\"a\": [1, 2], \"b\": null}".into()),
        ("sql injection", "'; DROP TABLE docs;--".into()),
        ("markdown fence bomb", "```\n```\n```\n".into()),
        ("unclosed fence", "# t\n\n```rust\nlet x = 1;\n".into()),
        ("backtick runs", "`".repeat(256)),
        ("heading spam", "######\n#####\n####\n###\n".into()),
        ("unclosed brackets", "[".repeat(4_096)),
        ("unclosed emphasis", "*".repeat(4_096)),
        ("html comment bomb", "<!--".repeat(512)),
        ("empty package spec", String::new()),
        ("at sign only", "@".into()),
        ("name without version", "base64@".into()),
        ("version without name", "@0.21.7".into()),
        ("double at", "base64@@0.21.7".into()),
        ("version with metachars", "base64@0.2*".into()),
        ("one megabyte word", "a".repeat(1_048_576)),
        ("one megabyte prose", "word ".repeat(200_000)),
        ("fifty thousand newlines", "\n".repeat(50_000)),
    ]
}

/// A package identity for chunker inputs (ids derive from it; values here
/// only need to be stable, not real).
fn pkg() -> PackageIdentity {
    PackageIdentity {
        package_id: "adversarial#test-pkg@0.0.0".into(),
        name: "test-pkg".into(),
        version: "0.0.0".into(),
        source: None,
        manifest_path: PathBuf::from("/tmp/adversarial/Cargo.toml"),
    }
}

fn md_file() -> MarkdownFile {
    MarkdownFile {
        rel_path: "README.md".into(),
        abs_path: PathBuf::from("/tmp/adversarial/README.md"),
        kind: SourceKind::CrateReadme,
    }
}

#[test]
fn search_survives_every_payload() {
    let retriever = common::retriever();
    let mut unexpected: Vec<String> = Vec::new();
    for (name, payload) in payloads() {
        let query = SearchQuery {
            limit: 8,
            ..SearchQuery::new(payload.clone())
        };
        match retriever.search(&query) {
            Ok(hits) => {
                assert!(
                    hits.len() <= 8,
                    "{name}: {} hits exceeds limit 8",
                    hits.len()
                );
                for hit in &hits {
                    assert_eq!(
                        DocumentId::from_raw(hit.id.as_str()),
                        Some(hit.id.clone()),
                        "{name}: hit id does not round-trip through from_raw"
                    );
                    // Same unit as the property suite: the snippet cap
                    // counts characters, not bytes.
                    assert!(
                        hit.snippet.chars().count() <= 320,
                        "{name}: snippet is {} chars",
                        hit.snippet.chars().count()
                    );
                }
            }
            // The only legitimate error is the structured empty-query error.
            Err(KnowledgeError::Engine(_)) => {}
            Err(other) => unexpected.push(format!("{name}: unexpected error: {other:?}")),
        }
    }
    assert!(
        unexpected.is_empty(),
        "unexpected errors: {unexpected:?}"
    );
}

#[test]
fn symbol_lookup_survives_every_payload() {
    let retriever = common::retriever();
    let mut unexpected: Vec<String> = Vec::new();
    for (name, payload) in payloads() {
        let query = SymbolQuery {
            limit: 8,
            ..SymbolQuery::new(payload.clone())
        };
        match retriever.symbol_lookup(&query) {
            Ok(infos) => {
                assert!(
                    infos.len() <= 8,
                    "{name}: {} results exceeds limit 8",
                    infos.len()
                );
                for info in &infos {
                    assert!(
                        DocumentId::from_raw(info.id.as_str()).is_some(),
                        "{name}: result id does not round-trip through from_raw"
                    );
                }
            }
            Err(other) => unexpected.push(format!("{name}: unexpected error: {other:?}")),
        }
    }
    assert!(
        unexpected.is_empty(),
        "unexpected errors: {unexpected:?}"
    );
}

#[test]
fn id_parsing_and_get_survive_every_payload() {
    let retriever = common::retriever();
    let mut unexpected: Vec<String> = Vec::new();

    // A known id still resolves to an intact document.
    let known = common::sample_id();
    let doc = retriever.get(&known).expect("sample document exists");
    assert_eq!(doc.id, known, "sample document id must round-trip");

    // Uppercase-hex ids are valid and must behave like lowercase ones:
    // well-formed, but (almost surely) unknown → DocumentNotFound.
    let upper = known.as_str().to_uppercase();
    let parsed = DocumentId::from_raw(&upper).expect("uppercase hex id must parse");
    assert_eq!(parsed, known, "from_raw must normalize to lowercase");
    match retriever.get(&parsed) {
        Ok(found) => assert_eq!(found.id, parsed),
        Err(KnowledgeError::DocumentNotFound(_)) => {}
        Err(other) => unexpected.push(format!("unexpected error for uppercase id: {other:?}")),
    }

    for (name, payload) in payloads() {
        match DocumentId::from_raw(payload.as_str()) {
            // Malformed shapes are rejected without panicking.
            None => {}
            Some(id) => match retriever.get(&id) {
                Ok(found) => assert_eq!(found.id, id, "{name}: got a different document"),
                Err(KnowledgeError::DocumentNotFound(_)) => {}
                Err(other) => {
                    unexpected.push(format!("{name}: unexpected error: {other:?}"));
                }
            },
        }
    }
    assert!(
        unexpected.is_empty(),
        "unexpected errors: {unexpected:?}"
    );
}

#[test]
fn garbage_filters_never_widen_results() {
    let retriever = common::retriever();

    // Self-check: none of the payloads may collide with a real package key
    // or item kind, otherwise the zero-hits assertions below would be vacuous.
    let keys = common::package_keys();
    let kinds = common::item_kinds();
    for (name, payload) in payloads() {
        let normalized = payload.trim().to_lowercase();
        assert!(
            !keys
                .iter()
                .any(|k| k.eq_ignore_ascii_case(normalized.trim())),
            "{name}: payload accidentally names a real package; pick a different payload"
        );
        assert!(
            !kinds.iter().any(|k| k == &normalized),
            "{name}: payload accidentally names a real item kind; pick a different payload"
        );
    }

    // The query alone must hit the fixture, so a widened filter is visible.
    let baseline = retriever
        .search(&SearchQuery::new("engine encode"))
        .expect("baseline search");
    assert!(
        !baseline.is_empty(),
        "baseline query must produce hits to make widening observable"
    );

    for (name, payload) in payloads() {
        let package_query = SearchQuery {
            packages: vec![payload.clone()],
            ..SearchQuery::new("engine encode")
        };
        let hits = retriever.search(&package_query).expect("filtered search");
        assert!(
            hits.is_empty(),
            "{name}: garbage-only package filter widened to {} hits",
            hits.len()
        );

        let kind_query = SearchQuery {
            item_kinds: vec![payload.clone()],
            ..SearchQuery::new("engine encode")
        };
        let hits = retriever.search(&kind_query).expect("filtered search");
        assert!(
            hits.is_empty(),
            "{name}: garbage-only item-kind filter widened to {} hits",
            hits.len()
        );
    }
}

#[test]
fn mixed_valid_and_garbage_filters_keep_valid_semantics() {
    let retriever = common::retriever();
    let query = SearchQuery {
        packages: vec![
            "base64".into(),
            "../../../etc/passwd".into(),
            String::new(),
            "base64@".into(),
        ],
        ..SearchQuery::new("engine encode")
    };
    let hits = retriever.search(&query).expect("search");
    assert!(
        !hits.is_empty(),
        "the valid entry in a mixed filter must still match"
    );
    for hit in &hits {
        // Only the valid spec may contribute hits.
        assert_eq!(
            hit.package_name, "base64",
            "hit outside the valid filter set"
        );
    }
}

#[test]
fn limit_edges_are_handled() {
    let retriever = common::retriever();
    for limit in [0usize, 1, 2, 50, 1_000_000, usize::MAX] {
        let hits = retriever
            .search(&SearchQuery {
                limit,
                ..SearchQuery::new("write_all")
            })
            .expect("search with edge limit");
        assert!(
            hits.len() <= limit.max(1),
            "search limit {limit}: {} hits",
            hits.len()
        );

        let infos = retriever
            .symbol_lookup(&SymbolQuery {
                limit,
                ..SymbolQuery::new("Writer")
            })
            .expect("symbol lookup with edge limit");
        assert!(
            infos.len() <= limit.max(1),
            "symbol limit {limit}: {} results",
            infos.len()
        );
    }
}

#[test]
fn chunker_survives_every_payload() {
    for (name, payload) in payloads() {
        let docs = chunk_markdown(&pkg(), &md_file(), &payload);
        // Deterministic: same input, same documents (same ids included).
        let again = chunk_markdown(&pkg(), &md_file(), &payload);
        assert!(docs == again, "{name}: chunking is not deterministic");

        let mut ids = HashSet::new();
        for doc in &docs {
            assert!(
                doc.text.len() <= MAX_CHUNK_CHARS + CHUNK_SLACK,
                "{name}: chunk is {} chars, exceeding MAX_CHUNK_CHARS ({MAX_CHUNK_CHARS}) + {CHUNK_SLACK}",
                doc.text.len()
            );
            assert!(
                ids.insert(doc.id.clone()),
                "{name}: chunker produced a duplicate id"
            );
            assert!(
                DocumentId::from_raw(doc.id.as_str()).is_some(),
                "{name}: chunk id is not a valid hex id"
            );
        }
    }
}
