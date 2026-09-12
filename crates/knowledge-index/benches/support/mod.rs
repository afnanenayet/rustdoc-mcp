//! Shared harness for the engine criterion benches (`../engine.rs`).
//!
//! Everything deterministic-but-slow happens once in [`build_state`]: the
//! fixture universe is resolved with `cargo metadata` (a subprocess — it
//! belongs to setup, never to a measured iteration), the corpus is built
//! from the committed prebuilt rustdoc artifacts and the fixture + registry
//! markdown, and a persistent index is built into a scratch `TempDir` that
//! `BenchState` keeps alive for the whole bench run and cleans up at exit.
//!
//! Determinism and hygiene rules enforced here:
//!
//! * every directory the engine writes to comes from `tempfile::tempdir()`;
//!   the harness never uses pipeline defaults (`<workspace>/.rust-knowledge`)
//!   or environment-configured locations;
//! * inputs are the committed fixture workspace, its prebuilt rustdoc JSON
//!   and the committed eval query set — no network, no nightly toolchain;
//! * `RustdocScope::All` additionally reads the READMEs of registry
//!   packages (anyhow, base64 x2) from the local cargo registry cache,
//!   exactly like the integration tests do. Those reads are part of the
//!   measured corpus-build stage.
//!
//! A future larger synthetic corpus plugs in behind the same shape: the
//! benches in `engine.rs` only consume [`BenchState`], so replacing how
//! `documents`/`meta` are produced does not redesign the suite.

use std::path::{Path, PathBuf};

use cargo_metadata::Package;
use knowledge_core::{
    DocumentId, KnowledgeDocument, KnowledgeRetriever, PackageIdentity, SearchQuery, SymbolQuery,
};
use knowledge_index::CargoUniverse;
use knowledge_index::corpus::{CorpusOptions, RustdocScope, build_corpus};
use knowledge_index::eval::parse_cases;
use knowledge_index::markdown::{self, MarkdownFile};
use knowledge_index::rustdoc::{PrebuiltRustdocProvider, RustdocProvider, normalize};
use knowledge_index::store::IndexMeta;
use knowledge_index::tantivy_index::{TantivyRetriever, build_index};
use tempfile::TempDir;

/// The committed fixture workspace (its own cargo workspace, excluded from
/// the root workspace).
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/demo-workspace")
}

/// The committed eval query set: the 21 representative queries.
fn eval_set_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/queries.toml")
}

/// One rustdoc parse input: the package identity plus its prebuilt JSON
/// artifact, with the document count pre-computed in setup (it drives the
/// docs/sec throughput of the parse benches).
pub struct RustdocInput {
    pub package: PackageIdentity,
    pub artifact: PathBuf,
    pub documents: usize,
}

/// One markdown chunking input: identity + file + pre-read text, with the
/// chunk count pre-computed in setup (drives docs/sec throughput).
pub struct MarkdownInput {
    pub identity: PackageIdentity,
    pub file: MarkdownFile,
    pub text: String,
    pub chunks: usize,
}

/// One eval case turned into a bench query with a stable benchmark id.
pub struct CaseQuery {
    /// Stable criterion id, e.g. `case_04_encode_urlsafe` (index prefix keeps
    /// ids unique; the slug keeps them readable). Editing
    /// `evals/queries.toml` breaks comparability — by design.
    pub id: String,
    /// The exact `SearchQuery` that `eval::run_eval` would issue for the
    /// case: same text, same limit (`max_rank.clamp(5, 10)`), no filters.
    pub query: SearchQuery,
}

/// Everything the benches need, built once per run.
pub struct BenchState {
    /// The resolved fixture universe (`cargo metadata` ran in setup).
    pub universe: CargoUniverse,
    /// The prebuilt-artifact provider the corpus-build stage uses.
    pub provider: PrebuiltRustdocProvider,
    /// One entry per prebuilt rustdoc artifact.
    pub rustdoc_inputs: Vec<RustdocInput>,
    /// One entry per discovered markdown file (fixture and registry
    /// packages alike, in `build_corpus`'s deterministic order).
    pub markdown_inputs: Vec<MarkdownInput>,
    /// The full normalized corpus (deterministic order, stable ids).
    pub documents: Vec<KnowledgeDocument>,
    /// Index metadata matching the corpus (the tests/eval.rs recipe).
    pub meta: IndexMeta,
    /// Scratch dir holding the persistent index; alive for the whole run and
    /// dropped when the bench binary exits. Never a default location.
    index_dir: TempDir,
    /// Retriever over the persistent index.
    pub retriever: TantivyRetriever,
    /// The eval queries, in file order, with stable bench ids.
    pub cases: Vec<CaseQuery>,
    /// The first eval query, for the startup bench's first-answer half.
    pub first_query: SearchQuery,
    /// The first eval query restricted to `demo-core`: the
    /// `knowledge_search` packages-filter path (`filter_clause`'s
    /// BooleanQuery MUST over the package term), validated in setup to
    /// return demo-core hits.
    pub filtered_query: SearchQuery,
    /// Fixed document id for the doc-get bench (deterministic SHA-256 of a
    /// known fixture symbol).
    pub get_id: DocumentId,
    /// symbol_lookup inputs, one per lookup path.
    pub symbol_exact: SymbolQuery,
    pub symbol_last_segment: SymbolQuery,
    pub symbol_conjunction: SymbolQuery,
}

impl BenchState {
    /// Path of the persistent index (valid for the whole bench run).
    pub fn index_path(&self) -> &Path {
        self.index_dir.path()
    }
}

/// Builds the shared bench state once. All engine work here is setup: it is
/// never inside a measured iteration. Fails loudly (via `expect`) if the
/// fixture inputs drift, so a broken bench is preferable to a silent miss.
pub fn build_state() -> BenchState {
    let manifest = fixture_dir().join("Cargo.toml");
    let universe =
        CargoUniverse::load(Some(&manifest)).expect("cargo metadata on the fixture workspace");
    let provider = PrebuiltRustdocProvider {
        dir: fixture_dir().join("prebuilt-rustdoc"),
    };

    // --- rustdoc parse inputs: what build_corpus would normalize ---
    // Scope All puts every resolved package in scope, mirroring the corpus
    // stage under bench; the prebuilt provider hands back the committed
    // artifacts.
    let packages: Vec<&Package> = universe.packages().collect();
    let generated = provider
        .generate(&universe, &packages)
        .expect("prebuilt rustdoc artifacts resolve");
    let mut rustdoc_inputs = Vec::new();
    for artifact in &generated.artifacts {
        let normalized = normalize(&artifact.package, &artifact.path, universe.workspace_root())
            .expect("prebuilt rustdoc artifact parses");
        rustdoc_inputs.push(RustdocInput {
            package: artifact.package.clone(),
            artifact: artifact.path.clone(),
            documents: normalized.documents.len(),
        });
    }
    rustdoc_inputs
        .first()
        .expect("prebuilt rustdoc artifacts are discoverable");

    // --- markdown chunking inputs, in build_corpus's package order ---
    let mut all_pkgs: Vec<&Package> = universe.packages().collect();
    all_pkgs.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    let mut markdown_inputs = Vec::new();
    for pkg in &all_pkgs {
        let identity = universe.identity(pkg);
        let readme = pkg.readme.as_ref().map(|p| p.as_std_path());
        for file in markdown::discover(&identity, readme) {
            let text = std::fs::read_to_string(&file.abs_path).expect("markdown file is readable");
            let chunks = markdown::chunk_markdown(&identity, &file, &text).len();
            markdown_inputs.push(MarkdownInput {
                identity: identity.clone(),
                file,
                text,
                chunks,
            });
        }
    }
    markdown_inputs
        .first()
        .expect("markdown files are discovered for the fixture universe");

    // --- the full corpus + metadata, the tests/eval.rs recipe ---
    let (documents, report) = build_corpus(
        &universe,
        &provider,
        &CorpusOptions {
            rustdoc_scope: RustdocScope::All,
        },
    )
    .expect("fixture corpus builds");
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
        built_at: "bench".into(),
        skipped: Vec::new(),
        warnings: Vec::new(),
    };

    // --- the persistent index the retrieval benches read ---
    let index_dir = tempfile::tempdir().expect("scratch index tempdir");
    build_index(index_dir.path(), &documents, &meta).expect("persistent index builds");
    let retriever = TantivyRetriever::open(index_dir.path()).expect("persistent index opens");

    // --- the 21 eval queries ---
    let raw = std::fs::read_to_string(eval_set_path()).expect("eval set is present");
    let parsed = parse_cases(&raw).expect("eval set parses");
    let mut cases = Vec::with_capacity(parsed.len());
    for (i, case) in parsed.iter().enumerate() {
        let limit = case.max_rank.clamp(5, 10); // mirrors eval::run_eval
        cases.push(CaseQuery {
            id: format!("case_{i:02}_{}", slug(&case.text)),
            query: SearchQuery {
                text: case.text.clone(),
                packages: Vec::new(),
                source_kinds: Vec::new(),
                item_kinds: Vec::new(),
                limit,
            },
        });
    }
    let first_case = cases.first().expect("eval set keeps its 21 cases");
    let first_query = first_case.query.clone();

    // --- the packages-filter search query ---
    // Same text and limit as the first eval query, restricted to
    // demo-core: the filter_clause path (BooleanQuery MUST over the
    // package term) that knowledge_search's packages parameter adds.
    // The unfiltered first case is its twin bench, so the pair
    // isolates the filter's cost.
    let filtered_query = SearchQuery {
        packages: vec!["demo-core".to_string()],
        ..first_case.query.clone()
    };
    let filtered_hits = expect_package_hits(&retriever, &filtered_query, "demo-core");

    // --- symbol_lookup inputs, one per path, validated now ---
    let symbol_exact = SymbolQuery::new("demo_core::encoding::encode_urlsafe");
    let symbol_last_segment = SymbolQuery::new("encode_urlsafe");
    // Deliberately not an exact symbol path: a qualified, multi-token query
    // with no symbol_exact hit — the conjunction clause (MUST over every
    // path token) is what ranks the real method above bare-segment ties.
    let symbol_conjunction = SymbolQuery::new("demo_core::writer::write_all");
    let exact_hits = expect_symbol_hits(&retriever, &symbol_exact);
    let last_hits = expect_symbol_hits(&retriever, &symbol_last_segment);
    let conjunction_hits = expect_symbol_hits(&retriever, &symbol_conjunction);

    // --- the fixed document id for the doc-get bench ---
    let target = documents
        .iter()
        .find(|d| d.symbol_path.as_deref() == Some("demo_core::encoding::encode_urlsafe"))
        .expect("fixture corpus contains demo_core::encoding::encode_urlsafe");
    let get_id = target.id.clone();
    retriever
        .get(&get_id)
        .expect("fixed document id resolves in the index");

    eprintln!(
        "bench setup: {} packages, {} rustdoc artifacts, {} markdown files, \
        {} documents, {} eval cases; symbol_lookup hits exact/last/conjunction: \
        {}/{}/{}; filtered demo-core search hits: {}",
        report.packages,
        rustdoc_inputs.len(),
        markdown_inputs.len(),
        documents.len(),
        cases.len(),
        exact_hits,
        last_hits,
        conjunction_hits,
        filtered_hits,
    );

    BenchState {
        universe,
        provider,
        rustdoc_inputs,
        markdown_inputs,
        documents,
        meta,
        index_dir,
        retriever,
        cases,
        get_id,
        symbol_exact,
        symbol_last_segment,
        symbol_conjunction,
        first_query,
        filtered_query,
    }
}

/// Runs one symbol_lookup at setup and fails loudly when it returns nothing,
/// so the benches never silently measure a miss path.
fn expect_symbol_hits(retriever: &TantivyRetriever, query: &SymbolQuery) -> usize {
    let hits = retriever
        .symbol_lookup(query)
        .expect("symbol lookup runs against the persistent fixture index");
    hits.first()
        .expect("symbol lookup must return fixture hits");
    hits.len()
}

/// Runs one package-filtered search at setup and fails loudly when it
/// returns no hit from the filtered package, so the filtered bench
/// never silently measures an empty result set.
fn expect_package_hits(retriever: &TantivyRetriever, query: &SearchQuery, package: &str) -> usize {
    let hits = retriever
        .search(query)
        .expect("filtered search runs against the persistent fixture index");
    hits.iter()
        .find(|h| h.package_name == package)
        .expect("filtered search must return hits from the filtered package");
    hits.len()
}

/// Slugifies query text into a stable, filesystem-safe id part.
fn slug(text: &str) -> String {
    let mut slug = String::with_capacity(24);
    let mut last_sep = true; // suppresses leading separators
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            slug.push('_');
            last_sep = true;
        }
    }
    if slug.ends_with('_') {
        slug.pop();
    }
    slug.chars().take(24).collect()
}
