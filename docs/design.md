# rust-knowledge design

A local, Cargo-aware documentation retrieval prototype for Rust monorepos.
Target users are coding agents (Claude Code, Codex) and humans; the product is a
small CLI + MCP server that answers "what API should I use / how does this
crate work / where is this documented?" from the **exact resolved Cargo
dependency universe** of a workspace, without the agent crawling
`~/.cargo/registry`, `target/`, or generated files.

## Goals

1. Reason about the packages Cargo actually resolved (via `cargo metadata`),
   never by guessing paths.
2. Treat natural-language docs (crate/module/item docs, READMEs, `docs/*.md`)
   and structured API docs (rustdoc JSON) as one corpus with provenance.
3. Progressive disclosure: compact search previews (few hundred chars), full
   documents only on demand (`doc_read`).
4. Deterministic, inspectable, boring: deterministic IDs, JSONL corpus dump,
   explicit index metadata, typed errors, `tracing` spans.
5. Cheap interactive queries against a persistent local Tantivy index; all
   expensive work happens at `index` time.

## Non-goals (v1)

- No rust-analyzer/SCIP, call graphs, or references.
- No vector/embedding retrieval (designed for, implemented later behind the
  same interface — see "Semantic retrieval (future)").
- No external services (no Elasticsearch/Qdrant/Postgres). Everything is
  local files.
- No incremental re-indexing; rebuilds are acceptable.
- No custom Cargo resolver, no `docs.rs` downloading, no Internet search.

## Upstream APIs investigated (2026-09, all verified first hand)

Findings below were established by reading the actual crate sources and by
generating artifacts, **not** from assumption. Versions pinned in the root
`Cargo.toml`.

### `cargo metadata` via `cargo_metadata` 0.23.1

- `MetadataCommand::new().manifest_path(p).exec()` runs `cargo metadata
  --format-version 1` (network access needed only if the graph is not yet
  fetched).
- `PackageId` is an **opaque newtype** (`repr` string, e.g.
  `registry+https://github.com/rust-lang/crates.io-index#serde@1.0.219`).
  Never parse it; use it as a map key and render via `Display`.
- `Package` gives: `id`, `name`, `version`, `source: Option<Source>`,
  `manifest_path` (absolute `Utf8PathBuf`), `readme: Option<Utf8PathBuf>`,
  `targets`, `dependencies`, `features`. `Source.repr` distinguishes
  `registry+` / `git+` / `path+`; `None` means workspace-local member.
- `Metadata.resolve` (`Resolve { root, nodes }`) is the resolved graph;
  `Node.deps` (`NodeDep { name, pkg, dep_kinds }`) accounts for renamed
  dependencies and target/platform gating; `Node.features` lists enabled
  features.
- Two versions of the same crate are distinct `PackageId`s and distinct
  `PackageIdentity`s. Package identity = `PackageId`, never crate name.

### Rustdoc JSON via `rustdoc-types` 0.61.0

Generated with nightly: `cargo rustdoc --lib -Z unstable-options
--output-format json` (rustdoc JSON remains **nightly-only** and unstable).
Empirically verified against nightly `1.100.0-nightly (a69a63265 2026-09-03)`:

- `format_version` is **61**, matching `rustdoc_types::FORMAT_VERSION == 61`.
  The parser checks this and fails with a diagnostic on mismatch. The JSON is
  a *versioned external format*; on upstream format bumps,
  `rustdoc-types` must be bumped in lockstep.
- `Crate` has **no `crate_name` field** (older tutorials show one); the crate
  name is taken from the cargo package, not the JSON.
- Item `Id` is a plain integer (`Id(pub u32)`), not the historical `"0:123"`
  string.
- `Item.kind` does not exist as a JSON field; the kind is the tag of
  `Item.inner` (`ItemEnum`). `rustdoc-types` provides `ItemEnum::kind()`.
- `Crate.index` contains **all** items (local plus referenced externals like
  `core`/`std` items reached through trait impls). Local items are those with
  `crate_id == 0`.
- `Crate.paths` (`ItemSummary`) is **not a complete map** — it is populated
  for items reachable from intra-doc links/trait impls. Full API enumeration
  requires walking `index[root] -> Module.items` recursively.
- Re-exports (`pub use`) are the primary API surface of re-export-heavy
  crates (tokio exposes `task::spawn_blocking` this way): `Use` items carry
  `name: null` on the `Item` (the effective name lives on `Use::name`, after
  any rename) and `Use::id` points at the target. The walker follows them and
  indexes targets under the re-export path — following this rule grew the
  dogfood corpus of this repository's own dependency graph by 3.3x.
- Deeply nested const-generic types (typenum) exceed serde's default
  recursion limit; artifacts are parsed with the limit disabled on a
  dedicated 64 MiB-stack thread, and `format_version` is scanned flat out of
  the raw text (probing via `serde_json::Value` hits the same limit, and the
  field name also occurs inside self-referential doc comments — the real
  field is the last colon-adjacent occurrence).
- `Span.filename` is relative to rustdoc's working directory (cargo runs
  rustdoc with cwd = workspace root). The ingester resolves spans against the
  workspace root.
- `Function` no longer has a `head` string; signatures are rendered from
  `FunctionSignature { inputs, output, abi }` + `FunctionHeader {is_const,
  is_unsafe, is_async}` + `Generics`.
- Derive-generated impls appear as items with `is_synthetic: true` — skipped.
- Docs live in `Item.docs` (Markdown, may be absent); intra-doc link targets
  in `Item.links: HashMap<String, Id>` resolve through `index`/`paths` to
  related symbols, including cross-crate ones.
- Command isolation: `cargo` invokes the `rustdoc` **found on PATH** (not
  toolchain-pinned). `cargo +nightly` works because the rustup shim prepends
  the toolchain bin dir to the child PATH. The provider therefore builds
  cargo invocations carefully (see `rustdoc::provider`).

### Tantivy 0.26.1

Local LSM index; used for weighted multi-field lexical retrieval:
`Schema::builder`, `Index::create_in_dir`/`open_in_dir`,
`QueryParser::for_index` with `set_field_boost`, `TermQuery`/`BooleanQuery`
for filters and exact identifier boosting, `SnippetGenerator` for compact
previews, stored fields give us `get(id)` without a second document store
(plus a JSONL corpus dump for inspectability). Empirical 0.26 notes that
differ from older tutorials:

- `Index::create_in_dir` does **not** create the directory — create it
  first (the builder does).
- `Document` is now a trait; the concrete type is `TantivyDocument`,
  built with `add_text(field, value)` mutations.
- `TopDocs::with_limit(n)` is not itself a `Collector` in 0.26:
  pass `TopDocs::with_limit(n).order_by_score()` to `searcher.search`.
- Text options are composed via `TEXT | STORED`-style flags; the body field
  uses the built-in `en_stem` tokenizer (see below).

### MCP via `rmcp` 3.2.0 (official Rust SDK)

- Pattern: `#[tool_router] impl Handler { #[tool(description = "...")] async
  fn tool(&self, Parameters(Req{..}): Parameters<Req>) -> ... }` plus an
  explicit `#[tool_handler] impl ServerHandler for Handler { fn get_info ... }`
  (the attribute fills in call_tool/list_tools; `ServerInfo` is
  `#[non_exhaustive]`, so construct it via `ServerInfo::new` +
  `.with_instructions`).
- `rmcp::serve_server(handler, transport::stdio())` runs on stdio; the
  error type is `rmcp::ErrorData`; `internal_error` takes a message plus
  an optional JSON payload; `CallToolResult::is_error` is
  `Option<bool>` in the 2026 protocol.
- `server` feature already includes `schemars` for tool input schemas.
- The retrieval core must not depend on MCP: `knowledge-mcp` is a thin
  adapter over `knowledge-core::KnowledgeRetriever`.

## Architecture

```
Cargo.toml / Cargo.lock ──cargo metadata──> CargoUniverse (package identities,
│        resolved graph)                    `knowledge-index::cargo`
├── rustdoc JSON generation (nightly)      `knowledge-index::rustdoc::provider`
│      └── parse + normalize (rustdoc-types) `knowledge-index::rustdoc::items`
├── README / docs/*.md chunking             `knowledge-index::markdown`
▼
KnowledgeDocument corpus (serde JSONL dump + typed docs)
▼
Tantivy index (schema in `knowledge-index::tantivy`-ish module: schema.rs)
▼
KnowledgeRetriever trait (sync; `knowledge-core`)
├── knowledge-cli  (figue over facet shapes; index/search/get/packages/dump-docs/eval/config-docs)
└── knowledge-mcp  (rmcp; knowledge_search/doc_read/symbol_lookup)
```

Crates (kept to four):

- `knowledge-core`: normalized data model, `DocumentId`, `SourceKind`,
  `SearchQuery`/`SearchHit`, `KnowledgeRetriever` trait. No cargo/rustdoc/
  tantivy/rmcp dependency — the eventual vector retriever plugs in here.
- `knowledge-index`: cargo ingestion, rustdoc provider + normalizer,
  markdown chunker, corpus builder, Tantivy index + retriever, orchestrator
  (`Indexer`), index metadata store. Integration-tested against a committed
  fixture workspace.
- `knowledge-cli` (bin `rust-knowledge`), `knowledge-mcp` (bin
  `knowledge-mcp`): thin frontends over the same engine.

## Data model (`knowledge-core`)

```rust
struct PackageIdentity { package_id, name, version, source: Option<String>,
                         manifest_path }
enum SourceKind { RustdocItem, RustdocModule, CrateReadme, MarkdownDocument }
struct KnowledgeDocument { id: DocumentId, package: PackageIdentity,
    source_kind, title, symbol_path: Option<String>, item_kind: Option<String>,
    section_path: Vec<String>, text: String,
    source_path: Option<PathBuf>, source_span: Option<SourceSpan>,
    related_symbols: Vec<String>, signature: Option<String> }
```

- `DocumentId`: lowercase hex of SHA-256 over a canonical identity string
  `"{package_id}\x1f{source_kind}\x1f{symbol_or_relpath}\x1f{section_identity}\x1f{chunk}"`
  — deterministic across rebuilds; independent of insertion order/vector
  offsets. `get(id)` looks it up by exact term.
- Rustdoc item docs keep symbol provenance (`tokio::task::spawn_blocking` style
  paths, kind, signature, span, resolved intra-doc link targets as
  `related_symbols`). Markdown chunks carry heading ancestry (`section_path`)
  and package/document identity.
- The index stores contextualized searchable text ("Package: name version /
  Symbol: ... / Kind: ..." prefixes are indexing context, not user output).

## Chunking

- Markdown: structural chunks per heading (preamble = its own chunk); adjacent
  paragraphs accumulate up to ~4 KiB, then split at block boundaries while
  retaining full heading ancestry on each chunk. Deterministic, no LLM.
- Rustdoc: one document per publicly visible documented item (module crate-root
  and modules included as `RustdocModule`); docs + rendered signature make up
  the text. Undocumented derive/synthetic noise is skipped; public items are
  kept even when undocumented (symbol lookup must work for them), with the
  signature as body.

## Search design

Tantivy fields (tokenized fields: `simple + lower_case`; the `body` prose
field additionally uses the built-in English stemmer `en_stem`, so "errors"
matches "error" and "buffered" matches "buffering" — a fix driven directly by
the Stage 5 eval; identifier fields stay unstemmed because exactness beats
recall for symbols). Query-time boosts:

| field            | boost | notes                                   |
|------------------|-------|-----------------------------------------|
| `symbol_path`    | 10    | tokenized; partial identifier matches    |
| `title`          | 5     |                                          |
| `section_text`   | 3     | joined heading ancestry                  |
| `signature`      | 3     | full signatures are searchable           |
| `package_name`   | 2.5   |                                           |
| `related_text`   | 1.5   | resolved intra-doc link targets          |
| `body`           | 1     | English stemmer                          |

Plus non-tokenized `symbol_exact` (full path) and `symbol_last` (last path
segment) fields: identifier-looking query tokens (CamelCase/snake_case/`::`)
get **term-query boosts (30/12, plain words 8)** layered on the parsed query,
so exact symbol queries dominate. Filters (`packages`, `source_kinds`,
`item_kinds`) are BooleanQuery must-clauses on raw string fields
(`package_name_raw`, `package_key` = "name@version"); MCP never exposes
Tantivy query syntax. Snippets: `SnippetGenerator` on the body field,
capped ~300 chars, with signature fallback for undocumented items.

`symbol_lookup` is a separate path (exact → last-segment → all-segments
conjunction on the tokenized symbol path, descending boosts), not generic
search.

## Evaluation (Stage 5)

`evals/queries.toml` holds 21 committed queries across the required
categories (known symbol, API discovery, conceptual, cross-package,
version-sensitive). Each case states acceptable contexts, an optional package
constraint and a max rank; the integration test asserts every case passes
(21/21, MRR ≈ 0.87; 19/21 land at rank 1). The set is the regression
harness for any retrieval change — the stemmer decision above was validated
exactly this way.

## Index layout

```
<workspace>/.rust-knowledge/            (or --index-dir; never committed)
├── index-meta.json                     (fingerprint + provenance, below)
├── tantivy/                            (the index)
├── corpus.jsonl                        (normalized docs, debug/inspectability)
└── cache/rustdoc/<name>-<version>.json (rustdoc artifacts, named by version so
                                         two versions of a crate never collide)
```

`index-meta.json` records: workspace root, Cargo.lock hash, cargo metadata
fingerprint, cargo/rustdoc versions, rustdoc format_version (61), index schema
version, package/doc counts, build timestamp. Structured so incremental
indexing can be added later without format breakage.

## Observability, errors, performance

- `tracing` spans: indexing stages (`cargo_metadata`, `rustdoc_generation`,
  `rustdoc_normalize`, `package_ingestion`, `markdown_discovery`,
  `markdown_ingestion`, `index_build`) and request paths (`search`,
  `symbol_lookup`, `doc_get`), with counts (packages discovered, artifacts
  parsed, docs normalized/skipped, index size) and `elapsed_ms` stage/request
  timings. User query text lives at debug level only; doc bodies are never
  logged.
- Both binaries initialize tracing through one shared layered helper
  (`knowledge_index::telemetry`): `Registry` → `EnvFilter` → formatting
  layer, always writing to **stderr** (stdout is a data channel — JSON-RPC
  for the MCP server, `--json` for the CLI). Filter precedence is
  `RUST_KNOWLEDGE_LOG` → `RUST_LOG` → a built-in default with per-target
  levels for noisy libraries; `RUST_KNOWLEDGE_LOG_FORMAT=json` selects the
  export-ready encoding. The init API accepts extra layers, so an
  OpenTelemetry/OTLP exporter slots in later behind a cargo feature with
  no redesign. See [observability.md](observability.md) for the env-var
  reference, the level policy, the span inventory, and the export seam.
- Typed `thiserror` errors carrying package/version/command/path/format
  version context, e.g. "failed to parse rustdoc JSON for {pkg}: format
  version {got} unsupported by parser {expected} (artifact: {path})".
- Indexing is the expensive step; search re-opens the persisted index,
  never invokes cargo/rustdoc, and does not re-parse rustdoc JSON.

## Staged plan (status)

- Stage 1 cargo universe: ingestion, `PackageIdentity`, `packages` CLI. Done.
- Stage 2 corpus: rustdoc provider + normalizer, markdown chunker,
  `dump-docs`. Done.
- Stage 3 lexical index: Tantivy schema/boosts, `index`/`search`/`get`/
  `symbol`. Done.
- Stage 4 MCP: three tools over the same retriever, tested in-process against
  a real client and smoke-tested over stdio. Done.
- Stage 5 eval: 21 committed queries over the fixture workspace with expected
  contexts and rank assertions (21/21, MRR ≈ 0.87). Done.
- Dogfood: the repository indexes its own 230-package dependency graph
  (36k documents); `symbol tokio::task::spawn_blocking` resolves with
  provenance, and cargo never gets invoked at query time.
- Stage 6 (future) semantic retrieval: not built, by design (below).

## Semantic retrieval (future design note)

`KnowledgeRetriever` stays the only retrieval interface; a vector retriever
would implement it (or wrap both) and fuse lexical + semantic rankings with
reciprocal-rank fusion. The normalized corpus (stable `DocumentId`, package
filters) already carries everything needed for a document->embedding mapping
and a local embedding cache; no Tantivy-specific assumptions leak into
`knowledge-core`. Do not build it until Stage 5's eval makes the benefit
measurable.

## Known instability / assumptions

- rustdoc JSON is nightly-only and versioned; toolchain drift breaks parsing —
  handled by format-version checks with clear diagnostics, and by pinning the
  toolchain in tests to the one that generated committed fixtures.
- `cargo rustdoc -p <spec>` needs `name@version` specs when two versions of a
  crate resolve simultaneously; artifacts are moved out of `target/doc/`
  immediately (same crate name at two versions would otherwise overwrite).
- Rustdoc scope for v1 defaults to workspace members + path deps
  (`--rustdoc-scope` can widen or disable); registry deps' Markdown is always
  ingested (cheap), registry rustdoc generation is best-effort.
