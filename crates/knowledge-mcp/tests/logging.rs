//! Pins MCP stdout purity end to end: the real server binary over real
//! stdio must put nothing on stdout except JSON-RPC responses, while all
//! logs (including trace-level ones) land on stderr as structured JSON.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// The message field of a JSON log record (empty when absent).
fn message(v: &Value) -> &str {
    v.get("fields")
        .and_then(|f| f.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// One MCP session: initialize, then two tool calls, then stdin EOF. The
/// server drains the queued requests and exits, so the whole exchange can be
/// written up front and read back after the process finishes.
fn session(index_dir: &Path) -> (Vec<Value>, Vec<Value>) {
    let messages = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"stdout-purity-test","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"knowledge_search","arguments":{"query":"write_all"}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"symbol_lookup","arguments":{"symbol":"Writer::write_all"}}}"#,
    ];
    let mut child = Command::new(env!("CARGO_BIN_EXE_knowledge-mcp"))
        .arg("--index-dir")
        .arg(index_dir)
        .env("RUST_LOG", "trace")
        .env("RUST_KNOWLEDGE_LOG_FORMAT", "json")
        .env_remove("RUST_KNOWLEDGE_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn knowledge-mcp");
    {
        let mut stdin = child.stdin.take().expect("stdin pipe");
        for message in messages {
            stdin
                .write_all(message.as_bytes())
                .expect("write request line");
            stdin.write_all(b"\n").expect("write newline");
        }
        stdin.flush().expect("flush stdin");
        // stdin drops here: the server sees EOF, drains, and exits.
    }
    let output = child.wait_with_output().expect("wait for server");
    assert!(
        output.status.success(),
        "server exited with {output:?} (stderr: {})",
        String::from_utf8_lossy(&output.stderr)
    );

    let parse_json_lines = |raw: &[u8]| -> Vec<Value> {
        String::from_utf8_lossy(raw)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("every line is valid JSON"))
            .collect()
    };
    let stdout = parse_json_lines(&output.stdout);
    let stderr = parse_json_lines(&output.stderr);
    (stdout, stderr)
}

#[test]
fn stdout_is_pure_jsonrpc_and_logs_land_on_stderr() {
    let index_dir = tempfile::tempdir().expect("tempdir");
    build_fixture_index(index_dir.path());
    let (stdout, stderr) = session(index_dir.path());

    // Every stdout line is a JSON-RPC response: jsonrpc 2.0 with a result
    // or an error — never a log line, a stray print, or plain text.
    assert!(stdout.len() >= 3, "expected 3 responses, got {stdout:?}");
    for line in &stdout {
        assert_eq!(line["jsonrpc"], "2.0", "not JSON-RPC 2.0: {line}");
        assert!(
            line.get("result").is_some() || line.get("error").is_some(),
            "response carries neither result nor error: {line}"
        );
    }
    let id_of = |v: &Value| v.get("id").cloned().unwrap_or(Value::Null);
    for expected in [1, 2, 3] {
        assert!(
            stdout.iter().any(|v| id_of(v) == serde_json::json!(expected)),
            "request id {expected} unanswered; responses: {stdout:?}"
        );
    }
    // The search answer really contains hits: the data channel works.
    let search = stdout
        .iter()
        .find(|v| id_of(v) == serde_json::json!(2))
        .expect("search response");
    let text = search["result"]["content"]
        .as_array()
        .and_then(|c| c.first())
        .map(|c| c["text"].as_str().unwrap_or_default())
        .unwrap_or_default();
    assert!(text.contains("write_all"), "search payload: {text}");

    // Every stderr line is a structured JSON log record.
    assert!(
        stderr.len() >= 4,
        "expected trace-level JSON logs on stderr, got {stderr:?}"
    );
    for line in &stderr {
        assert!(
            line.get("level").is_some() && line.get("target").is_some(),
            "not a structured log record: {line}"
        );
    }


    // Startup telemetry: document count, no user text.
    let serving = stderr
        .iter()
        .find(|v| message(v) == "serving knowledge MCP on stdio")
        .expect("serving event on stderr");
    assert!(
        serving["fields"]["documents"].as_u64().is_some(),
        "serving event lacks documents: {serving}"
    );

    // Request telemetry: hits + elapsed_ms at info, user text only in the
    // debug span fields.
    let search_done = stderr
        .iter()
        .find(|v| message(v) == "search done")
        .expect("search done event on stderr");
    assert!(
        search_done["fields"]["hits"].as_u64().is_some(),
        "search done lacks hits: {search_done}"
    );
    assert!(
        search_done["fields"]["elapsed_ms"].as_u64().is_some(),
        "search done lacks elapsed_ms: {search_done}"
    );
    assert_eq!(
        search_done["span"]["query"].as_str(),
        Some("write_all"),
        "search span carries the query: {search_done}"
    );
    assert_eq!(search_done["span"]["name"].as_str(), Some("search"));

    let symbol_done = stderr
        .iter()
        .find(|v| message(v) == "symbol lookup done")
        .expect("symbol lookup done event on stderr");
    assert!(
        symbol_done["fields"]["hits"].as_u64().is_some(),
        "symbol lookup done lacks hits: {symbol_done}"
    );
    assert_eq!(
        symbol_done["span"]["symbol"].as_str(),
        Some("Writer::write_all"),
        "symbol_lookup span carries the symbol: {symbol_done}"
    );

    // Log lines never leak to the data channel.
    for line in &stdout {
        let text = line.to_string();
        assert!(
            !text.contains("search done") && !text.contains("serving knowledge"),
            "log output leaked to stdout: {text}"
        );
    }
}
