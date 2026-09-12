//! Criterion benchmarks over the real engine stages, on deterministic
//! inputs.
//!
//! Every workload runs against the committed fixture workspace
//! (`fixtures/demo-workspace`) with its prebuilt rustdoc artifacts — the
//! same recipe the integration tests use — and writes only to tempfile
//! scratch directories (never `.rust-knowledge`, never a default or
//! environment-configured index dir). No network, no nightly toolchain.
//!
//! Run with `cargo bench -p knowledge-index`; filter with
//! `cargo bench -p knowledge-index --bench engine -- <regex>`; list
//! with `--bench engine -- --list` (the scoping keeps trailing
//! arguments away from the package's libtest-harness lib bench target,
//! which rejects criterion's own flags — see the README's Running
//! section). See `benches/README.md` for per-bench documentation,
//! baseline comparison and noise guidance.
//!
//! One process, one [`BenchState`]: the expensive deterministic setup
//! (`cargo metadata`, corpus build, persistent index build) happens once
//! in `main` and is shared by every group, so no measured iteration ever
//! contains subprocess spawns. The custom `main` (instead of
//! `criterion_main!`) exists so the scratch `TempDir`s are dropped — and
//! their directories cleaned up — when the run finishes.

mod support;

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput};
use knowledge_core::KnowledgeRetriever;
use knowledge_index::corpus::{CorpusOptions, RustdocScope, build_corpus};
use knowledge_index::markdown;
use knowledge_index::rustdoc::normalize;
use knowledge_index::tantivy_index::{TantivyRetriever, build_index};

use support::{BenchState, build_state};

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    let state = build_state();
    corpus_benches(&mut criterion, &state);
    index_build_benches(&mut criterion, &state);
    search_benches(&mut criterion, &state);
    symbol_lookup_benches(&mut criterion, &state);
    doc_get_benches(&mut criterion, &state);
    startup_benches(&mut criterion, &state);
    // `state` drops here: the persistent index's scratch dir is cleaned up.
    criterion.final_summary();
}

/// Corpus-stage benches: what it costs to turn the committed fixture
/// inputs into the normalized document set.
///
/// Real workload: `rust-knowledge index` on a small monorepo. A regression
/// means slower re-indexing after every workspace or dependency change,
/// which users feel as `rust-knowledge index` latency and as stale-index
/// pressure on the MCP server.
///
/// Noise factors: artifact and markdown file reads (page-cached after the
/// first iteration) and, for `build_full`, registry README reads plus the
/// deterministic sort. Iterations run tens-to-hundreds of milliseconds, so
/// the group trades sample count for sample length: sample_size 10,
/// measurement 10 s, warm-up 1 s.
fn corpus_benches(c: &mut Criterion, state: &BenchState) {
    let mut group = c.benchmark_group("corpus");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.warm_up_time(Duration::from_secs(1));

    // Stage: rustdoc JSON parse + normalize, one bench per prebuilt
    // artifact (docs/sec). This is the per-artifact share of
    // `build_corpus`'s rustdoc stage, file read included — exactly what
    // the engine pays per artifact.
    for input in &state.rustdoc_inputs {
        group.throughput(Throughput::Elements(input.documents as u64));
        group.bench_function(
            format!("rustdoc_parse/{}", file_stem(&input.artifact)),
            |b| {
                b.iter(|| {
                    let normalized = normalize(
                        &input.package,
                        &input.artifact,
                        state.universe.workspace_root(),
                    )
                    .expect("rustdoc artifact parses");
                    black_box(normalized.documents.len())
                })
            },
        );
    }

    // Stage: markdown chunking, one bench per discovered file
    // (docs/sec). File reads are deliberately outside this stage (they
    // are covered by `build_full`): this isolates the pulldown-cmark
    // parse + chunk-emission cost.
    for input in &state.markdown_inputs {
        group.throughput(Throughput::Elements(input.chunks as u64));
        group.bench_function(
            format!(
                "markdown_chunk/{}@{}/{}",
                input.identity.name, input.identity.version, input.file.rel_path
            ),
            |b| {
                b.iter(|| {
                    let docs = markdown::chunk_markdown(&input.identity, &input.file, &input.text);
                    black_box(docs.len())
                })
            },
        );
    }

    // Stage: the whole corpus assembly (rustdoc parse + markdown chunk +
    // registry README reads + deterministic sort), docs/sec.
    group.throughput(Throughput::Elements(state.documents.len() as u64));
    group.bench_function("build_full", |b| {
        b.iter(|| {
            let (documents, report) = build_corpus(
                &state.universe,
                &state.provider,
                &CorpusOptions {
                    rustdoc_scope: RustdocScope::All,
                },
            )
            .expect("fixture corpus builds");
            black_box((documents.len(), report.markdown_files))
        })
    });

    group.finish();
}

/// Index-build bench: `build_index` from the pre-built corpus into a
/// fresh scratch directory, per iteration.
///
/// Real workload: the write side of `rust-knowledge index`. The measured
/// call includes everything a rebuild pays: tantivy segment writes,
/// commit, `wait_merging_threads` (multithreaded merges), the
/// corpus.jsonl write and the index-meta.json save. A regression means
/// slower `rust-knowledge index` runs and slower cold starts for the
/// MCP server. Docs/sec includes the commit cost by design.
///
/// Noise factors: the dominant noise source in the suite — disk I/O plus
/// tantivy's multithreaded merge behavior (thread count is not engine-
/// configurable today). sample_size 10 / measurement 20 s trades
/// precision for practicality (the 20 s leaves room for ten
/// multi-hundred-millisecond samples; 10 s under-sampled in the first
/// smoke run). The scratch tempdir is created in
/// `iter_batched` setup and dropped outside the timed region
/// (`BatchSize::PerIteration`), so directory creation and rm -rf
/// teardown never pollute the measurement.
fn index_build_benches(c: &mut Criterion, state: &BenchState) {
    let mut group = c.benchmark_group("index_build");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    group.warm_up_time(Duration::from_secs(1));
    group.throughput(Throughput::Elements(state.documents.len() as u64));

    group.bench_function("from_scratch_dir", |b| {
        b.iter_batched(
            || tempfile::tempdir().expect("scratch tempdir"),
            |dir| {
                build_index(dir.path(), &state.documents, &state.meta)
                    .expect("index builds into the scratch dir");
                dir
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

/// The 21-query search workload: the committed eval set as a
/// representative mix of known-symbol queries, natural-language questions
/// and version-sensitive lookups. Each case is benched on its own (stable
/// ids; regressions stay attributable to a query shape) and once together
/// as the mixed workload (all 21 queries per iteration, queries/sec).
/// One extra bench covers the packages-filter path: the first eval query
/// with `packages: ["demo-core"]` — the `filter_clause` (BooleanQuery
/// MUST over the package term) cost every filtered `knowledge_search`
/// call adds on top of the same text/limit its unfiltered twin pays.
///
/// Real workload: every `knowledge_search` MCP tool call / CLI search,
/// with the exact `SearchQuery` `eval::run_eval` would issue (same text,
/// same limit); the filtered twin matches the MCP tool's packages
/// parameter instead. A regression means slower answers for every agent
/// using the server.
///
/// Noise factors: essentially pure CPU (query construction, tantivy
/// search, snippet generation) on a warm page cache — the steadiest
/// benches in the suite. sample_size 100 / measurement 3 s / warm-up 1 s.
fn search_benches(c: &mut Criterion, state: &BenchState) {
    let mut group = c.benchmark_group("search_eval");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    let retriever = &state.retriever;
    for case in &state.cases {
        // One query per iteration: throughput is queries/sec.
        group.throughput(Throughput::Elements(1));
        group.bench_function(case.id.as_str(), |b| {
            b.iter(|| {
                let hits = retriever.search(&case.query).expect("fixture search");
                black_box(hits.len())
            })
        });
    }

    // The packages-filter path: the first eval query restricted to
    // demo-core — the filter_clause cost (BooleanQuery MUST over the
    // package term) that knowledge_search's packages parameter adds.
    // Comparing against the unfiltered first-case bench isolates the
    // filter's share of the cost.
    group.throughput(Throughput::Elements(1));
    group.bench_function("package_filtered", |b| {
        b.iter(|| {
            let hits = retriever
                .search(&state.filtered_query)
                .expect("filtered fixture search");
            black_box(hits.len())
        })
    });

    // The mixed workload: all 21 queries per iteration.
    group.throughput(Throughput::Elements(state.cases.len() as u64));
    group.bench_function("mixed_workload", |b| {
        b.iter(|| {
            for case in &state.cases {
                let hits = retriever.search(&case.query).expect("fixture search");
                black_box(hits.len());
            }
        })
    });

    group.finish();
}

/// The three `symbol_lookup` paths, as agents issue them:
///
/// * `exact` — a fully-qualified symbol (`demo_core::encoding::
///   encode_urlsafe`): the untokenized `symbol_exact` term is the hit.
/// * `bare_last_segment` — a bare identifier (`encode_urlsafe`): the
///   `symbol_last` term does the ranking.
/// * `qualified_conjunction` — a qualified multi-token path that is NOT
///   an exact symbol (`demo_core::writer::write_all` — the real path has
///   the `Writer` segment): the conjunction clause (MUST over every path
///   token) is what ranks the actual method above bare last-segment
///   ties. This is the "slightly misremembered path" shape.
///
/// Real workload: the `symbol_lookup` MCP tool. A regression means slower
/// exact-symbol answers. Noise factors: CPU-only, like search_eval.
fn symbol_lookup_benches(c: &mut Criterion, state: &BenchState) {
    let mut group = c.benchmark_group("symbol_lookup");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    let retriever = &state.retriever;
    let paths = [
        ("exact", &state.symbol_exact),
        ("bare_last_segment", &state.symbol_last_segment),
        ("qualified_conjunction", &state.symbol_conjunction),
    ];
    for (name, query) in paths {
        // One lookup per iteration: throughput is lookups/sec.
        group.throughput(Throughput::Elements(1));
        group.bench_function(name, |b| {
            b.iter(|| {
                let infos = retriever.symbol_lookup(query).expect("symbol lookup");
                black_box(infos.len())
            })
        });
    }

    group.finish();
}

/// Document get by fixed id: the `doc_read` MCP tool path — a term query
/// on the id field plus stored-field reconstruction of the full document.
///
/// The id is the deterministic SHA-256 identity of a known fixture symbol
/// (`demo_core::encoding::encode_urlsafe`), resolved once in setup, so
/// the bench always measures the hit path. A regression means slower
/// full-document fetches after a search. Noise factors: CPU-only.
fn doc_get_benches(c: &mut Criterion, state: &BenchState) {
    let mut group = c.benchmark_group("doc_get");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    let retriever = &state.retriever;
    let id = &state.get_id;
    // One fetch per iteration: throughput is documents/sec.
    group.throughput(Throughput::Elements(1));
    group.bench_function("by_fixed_id", |b| {
        b.iter(|| {
            let doc = retriever.get(id).expect("fixed document id resolves");
            black_box(doc.text.len())
        })
    });

    group.finish();
}

/// Index open + first query: the server startup path. Each iteration
/// opens a fresh `TantivyRetriever` over the persistent index (meta load,
/// schema check, tantivy open, reader creation + reload) and issues the
/// first eval query. The retriever is dropped inside the iteration; that
/// tiny teardown is measured too.
///
/// Real workload: what the MCP server pays once at startup before it can
/// answer anything. A regression means slower server cold starts; the
/// per-case search benches isolate the query share of this number.
/// Noise factors: meta + segment reads are page-cached after the first
/// iteration — the steady state a long-lived server sees. Cold-cache
/// first opens will be slower; that is documented, not benched.
fn startup_benches(c: &mut Criterion, state: &BenchState) {
    let mut group = c.benchmark_group("startup");
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(5));
    group.warm_up_time(Duration::from_secs(1));

    let index_path = state.index_path();
    let first_query = &state.first_query;
    // One open+query per iteration: throughput is startups/sec.
    group.throughput(Throughput::Elements(1));
    group.bench_function("open_and_first_query", |b| {
        b.iter(|| {
            let retriever = TantivyRetriever::open(index_path)
                .expect("persistent index opens for the startup bench");
            let hits = retriever.search(first_query).expect("first fixture query");
            black_box(hits.len())
        })
    });

    group.finish();
}

/// File stem of a rustdoc artifact path, used as the stable per-artifact
/// bench id (e.g. `anyhow-1.0.104`).
fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("artifact")
        .to_string()
}
