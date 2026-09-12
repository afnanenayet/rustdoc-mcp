//! End-to-end logging behavior of the rust-knowledge binary: filter
//! precedence (RUST_KNOWLEDGE_LOG -> RUST_LOG -> built-in default), JSON
//! log mode, the level policy for user query text, and stdout purity for
//! data output. Every run spawns the real binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use knowledge_index::CargoUniverse;
use knowledge_index::corpus::{CorpusOptions, RustdocScope, build_corpus};
use knowledge_index::rustdoc::PrebuiltRustdocProvider;
use knowledge_index::store::IndexMeta;
use knowledge_index::tantivy_index::build_index;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/demo-workspace")
}

fn build_fixture_index(dir: &Path) {
    let universe =
        CargoUniverse::load(Some(&fixture_dir().join("Cargo.toml"))).expect("cargo metadata");
    let provider = PrebuiltRustdocProvider {
        dir: fixture_dir().join("prebuilt-rustdoc"),
    };
    let (documents, report) = build_corpus(
        &universe,
        &provider,
        &CorpusOptions {
            rustdoc_scope: RustdocScope::All,
        },
    )
    .expect("corpus");
    let meta = IndexMeta {
        schema_version: IndexMeta::supported_schema(),
        workspace_root: universe.workspace_root().to_path_buf(),
        lock_hash: universe.lock_hash(),
        metadata_fingerprint: universe.fingerprint(),
        cargo_version: report.cargo_version.clone(),
        toolchain: None,
        rustdoc_format_version: report.rustdoc_format_version,
        rustdoc_scope: "All".into(),
        package_count: report.packages,
        document_count: documents.len(),
        built_at: "test".into(),
        skipped: Vec::new(),
        warnings: Vec::new(),
    };
    build_index(dir, &documents, &meta).expect("build index");
}

/// Runs the real binary with a clean tracing environment: the three
/// telemetry env vars are removed first, then overrides applied, so the
/// tests do not depend on the ambient environment.
fn run(index_dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rust-knowledge"));
    cmd.arg("--index-dir")
        .arg(index_dir)
        .args(args)
        .env_remove("RUST_KNOWLEDGE_LOG")
        .env_remove("RUST_LOG")
        .env_remove("RUST_KNOWLEDGE_LOG_FORMAT");
    for (name, value) in env {
        cmd.env(name, value);
    }
    cmd.output().expect("run rust-knowledge")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Parses every non-empty stderr line as a JSON log record.
fn json_log_records(output: &Output) -> Vec<Value> {
    stderr_of(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("stderr line is JSON"))
        .collect()
}

/// The first record whose message field equals `message`.
fn log_event<'a>(records: &'a [Value], message: &str) -> &'a Value {
    records
        .iter()
        .find(|v| {
            v.get("fields")
                .and_then(|f| f.get("message"))
                .and_then(Value::as_str)
                .is_some_and(|m| m == message)
        })
        .expect("log event exists")
}

#[test]
fn json_mode_carries_span_fields_and_keeps_stdout_pure() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());
    let output = run(
        index_dir.path(),
        &["search", "write_all", "--json"],
        &[("RUST_LOG", "trace"), ("RUST_KNOWLEDGE_LOG_FORMAT", "json")],
    );
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));

    // stdout carries only search hits, never log output.
    let stdout = stdout_of(&output);
    assert!(!stdout.contains("search done"), "log leaked to stdout");
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let hit: Value = serde_json::from_str(line).expect("stdout line is a JSON hit");
        assert!(hit.get("id").is_some(), "hit shape: {hit}");
        assert!(hit.get("package_name").is_some(), "hit shape: {hit}");
    }

    // stderr carries structured JSON logs with span + event fields.
    let records = json_log_records(&output);
    let search_done = log_event(&records, "search done");
    assert!(
        search_done["fields"]["hits"].as_u64().is_some(),
        "search done lacks hits: {search_done}"
    );
    assert!(
        search_done["fields"]["elapsed_ms"].as_u64().is_some(),
        "search done lacks elapsed_ms: {search_done}"
    );
    assert_eq!(search_done["span"]["name"].as_str(), Some("search"));
    assert_eq!(search_done["span"]["query"].as_str(), Some("write_all"));
}

#[test]
fn default_info_omits_user_query_text() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());
    let output = run(index_dir.path(), &["search", "write_all", "--json"], &[]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stderr = stderr_of(&output);
    assert!(stderr.contains("search done"), "stderr: {stderr}");
    assert!(
        !stderr.contains("write_all"),
        "query text must not appear at default info level: {stderr}"
    );
    // Data output is unaffected.
    assert!(stdout_of(&output).contains("write_all"));
}

#[test]
fn verbose_flag_bumps_the_default_filter() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());
    let output = run(index_dir.path(), &["-v", "search", "write_all", "--json"], &[]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("write_all"),
        "-v must enable the debug search span (query text): {stderr}"
    );
}

#[test]
fn malformed_directive_degrades_to_the_default() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());
    let output = run(
        index_dir.path(),
        &["search", "write_all", "--json"],
        &[("RUST_KNOWLEDGE_LOG", "info=bogus")],
    );
    assert!(
        output.status.success(),
        "startup must survive a malformed directive; stderr: {}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("malformed log filter directive"),
        "expected the fallback warning: {stderr}"
    );
    assert!(
        stderr.contains("search done"),
        "default filter active: {stderr}"
    );
    assert!(
        stdout_of(&output).contains("write_all"),
        "search still works after the fallback"
    );
}

#[test]
fn rust_knowledge_log_beats_rust_log() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());
    let output = run(
        index_dir.path(),
        &["search", "write_all", "--json"],
        &[("RUST_KNOWLEDGE_LOG", "info"), ("RUST_LOG", "trace")],
    );
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(
        !stderr_of(&output).contains("write_all"),
        "RUST_KNOWLEDGE_LOG=info must win over RUST_LOG=trace"
    );
}

#[test]
fn rust_log_is_honored_without_the_compat_var() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());
    let output = run(
        index_dir.path(),
        &["search", "write_all", "--json"],
        &[("RUST_LOG", "debug")],
    );
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert!(
        stderr_of(&output).contains("write_all"),
        "RUST_LOG=debug must enable the debug search span"
    );
}

#[test]
fn get_span_is_visible_end_to_end() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());

    // Search first for a real document id (derived, not hardcoded).
    let search = run(
        index_dir.path(),
        &["search", "write_all", "--json"],
        &[("RUST_LOG", "trace"), ("RUST_KNOWLEDGE_LOG_FORMAT", "json")],
    );
    assert!(search.status.success(), "stderr: {}", stderr_of(&search));
    let id = stdout_of(&search)
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|hit| {
            hit.get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("search hit id");

    let get = run(
        index_dir.path(),
        &["get", id.as_str(), "--json"],
        &[("RUST_LOG", "trace"), ("RUST_KNOWLEDGE_LOG_FORMAT", "json")],
    );
    assert!(get.status.success(), "stderr: {}", stderr_of(&get));

    // stdout is the full JSON document (pretty-printed, one object).
    let doc: Value =
        serde_json::from_str(&stdout_of(&get)).expect("get stdout is one JSON document");
    assert_eq!(doc.get("id").and_then(Value::as_str), Some(id.as_str()));

    let records = json_log_records(&get);
    let retrieved = log_event(&records, "document retrieved");
    assert!(
        retrieved["fields"]["elapsed_ms"].as_u64().is_some(),
        "document retrieved lacks elapsed_ms: {retrieved}"
    );
    assert_eq!(retrieved["span"]["name"].as_str(), Some("doc_get"));
    assert_eq!(retrieved["span"]["id"].as_str(), Some(id.as_str()));
}

#[test]
fn indexing_stage_spans_are_visible_in_json_output() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    let output = run(
        index_dir.path(),
        &[
            "index",
            "--manifest-path",
            fixture_dir().join("Cargo.toml").to_str().expect("path"),
            "--prebuilt-rustdoc",
            fixture_dir().join("prebuilt-rustdoc").to_str().expect("path"),
        ],
        &[("RUST_LOG", "trace"), ("RUST_KNOWLEDGE_LOG_FORMAT", "json")],
    );
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));

    let records = json_log_records(&output);

    let built_corpus = log_event(&records, "built corpus");
    assert!(
        built_corpus["fields"]["documents"].as_u64().is_some(),
        "built corpus lacks documents: {built_corpus}"
    );
    assert!(
        built_corpus["fields"]["elapsed_ms"].as_u64().is_some(),
        "built corpus lacks elapsed_ms: {built_corpus}"
    );
    assert_eq!(
        built_corpus["span"]["name"].as_str(),
        Some("package_ingestion")
    );
    assert!(
        built_corpus["span"]["packages"].as_u64().is_some(),
        "package_ingestion span lacks packages: {built_corpus}"
    );

    let index_built = log_event(&records, "index built");
    assert!(
        index_built["fields"]["elapsed_ms"].as_u64().is_some(),
        "index built lacks elapsed_ms: {index_built}"
    );
    assert_eq!(index_built["span"]["name"].as_str(), Some("index_build"));

    let complete = log_event(&records, "indexing complete");
    assert!(
        complete["fields"]["elapsed_ms"].as_u64().is_some(),
        "indexing complete lacks elapsed_ms: {complete}"
    );
}
