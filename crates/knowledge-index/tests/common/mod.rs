//! Shared fixture index for knowledge-index integration tests.
//!
//! Each `tests/*.rs` binary includes this module and gets the same
//! process-wide fixture index: `cargo metadata` + corpus build + tantivy
//! build run once per binary instead of once per test. The `TempDir` lives
//! inside the static, so the index files outlive every test (statics never
//! drop — the same leak semantics the previous per-test `mem::forget` had).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use knowledge_core::DocumentId;
use knowledge_index::CargoUniverse;
use knowledge_index::corpus::{CorpusOptions, RustdocScope, build_corpus};
use knowledge_index::rustdoc::PrebuiltRustdocProvider;
use knowledge_index::store::IndexMeta;
use knowledge_index::tantivy_index::{TantivyRetriever, build_index};

pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/demo-workspace")
}

/// Process-wide fixture: owns the index directory and the retriever.
struct Fixture {
    /// Keeps the on-disk index alive for the whole process.
    _dir: tempfile::TempDir,
    retriever: TantivyRetriever,
    /// Distinct `name` and `name@version` keys that occur in the corpus.
    package_keys: Vec<String>,
    /// Id of a document that is known to exist in the index.
    sample_id: DocumentId,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn build_fixture() -> Fixture {
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
    let retriever = TantivyRetriever::open(dir.path()).expect("open index");

    let mut package_keys: Vec<String> = Vec::new();
    for doc in &documents {
        for key in [doc.package.name.clone(), doc.package.display()] {
            if !package_keys.contains(&key) {
                package_keys.push(key);
            }
        }
    }
    package_keys.sort();

    let sample_id = documents
        .iter()
        .find(|d| d.symbol_path.as_deref() == Some("demo_core::runtime::offload_blocking"))
        .or_else(|| documents.first())
        .expect("fixture corpus is not empty")
        .id
        .clone();

    Fixture {
        _dir: dir,
        retriever,
        package_keys,
        sample_id,
    }
}

/// The shared retriever over the committed fixture corpus.
pub fn retriever() -> &'static TantivyRetriever {
    &FIXTURE.get_or_init(build_fixture).retriever
}

/// Distinct `name` and `name@version` package keys present in the corpus.
pub fn package_keys() -> &'static [String] {
    &FIXTURE.get_or_init(build_fixture).package_keys
}

/// Id of a document that is known to exist in the fixture index.
pub fn sample_id() -> DocumentId {
    FIXTURE.get_or_init(build_fixture).sample_id.clone()
}
