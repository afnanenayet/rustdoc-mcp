//! Shared fixture index and in-memory transport for knowledge-mcp tests.
//!
//! The corpus + index build runs once per test process (OnceLock over the
//! index directory); each test opens the index (cheap) and serves its own
//! client over a fresh duplex transport. Proptest closures need to call the
//! async transport synchronously, so tests build an explicit current-thread
//! runtime and drive it with block_on — never blocking from inside the
//! runtime.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use knowledge_index::CargoUniverse;
use knowledge_index::corpus::{CorpusOptions, RustdocScope, build_corpus};
use knowledge_index::rustdoc::PrebuiltRustdocProvider;
use knowledge_index::store::IndexMeta;
use knowledge_index::tantivy_index::{TantivyRetriever, build_index};
use knowledge_mcp::KnowledgeServer;
use rmcp::model::CallToolRequestParams;
use rmcp::{ClientHandler, RoleClient, ServiceExt, service::RunningService};
use serde_json::{Value, json};

pub struct NoopClient;
impl ClientHandler for NoopClient {}

static INDEX_DIR: OnceLock<PathBuf> = OnceLock::new();

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/demo-workspace")
}

fn build_index_dir() -> PathBuf {
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
    let dir = tempfile::tempdir().expect("tempdir");
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
    build_index(dir.path(), &documents, &meta).expect("build index");
    let index_dir = dir.path().to_path_buf();
    // Leak the TempDir: statics never drop, and the index files must
    // outlive every test in the process.
    #[expect(
        clippy::mem_forget,
        reason = "keep the on-disk index alive for the whole test process"
    )]
    std::mem::forget(dir);
    index_dir
}

/// Directory of the process-wide fixture index (built once).
pub fn shared_index_dir() -> &'static Path {
    INDEX_DIR.get_or_init(build_index_dir)
}

/// Opens the shared fixture index. Cheap: the corpus + index build ran once.
pub fn fixture_retriever() -> TantivyRetriever {
    TantivyRetriever::open(shared_index_dir()).expect("open shared fixture index")
}

/// Serves one client over a fresh in-memory duplex transport.
pub async fn serve(retriever: TantivyRetriever) -> RunningService<RoleClient, NoopClient> {
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    let server = KnowledgeServer::new(retriever);
    let server_task =
        tokio::spawn(async move { server.serve(server_transport).await.expect("server serves") });
    let client = NoopClient
        .serve(client_transport)
        .await
        .expect("client serves");
    // Keep the server task alive for the lifetime of the test.
    #[expect(clippy::mem_forget, reason = "keep the server task alive for the test")]
    std::mem::forget(server_task);
    client
}

/// Tool-call parameters as they arrive from an LLM client.
pub fn tool_params(name: &str, arguments: Value) -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": name,
        "arguments": arguments,
    }))
    .expect("params")
}
