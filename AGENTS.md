# Instructions for coding agents working in this repository

`rust-knowledge` is a local, Cargo-aware documentation retrieval engine for Rust
monorepos. It answers "what API should I use / how does this crate work / where
is this documented?" from the **exact resolved Cargo dependency universe** of a
workspace — never by crawling `~/.cargo/registry`, `target/`, or generated files.
It ships as a CLI (`rust-knowledge`) and an MCP server (`knowledge-mcp`).

## Investigating Rust dependencies

Do not recursively search `~/.cargo/registry` or target directories. The
rust-knowledge MCP index already covers the exact resolved Cargo dependency
graph. Use the MCP tools in this order:

1. `knowledge_search` — search documentation (READMEs, guides, rustdoc) for APIs
   or conceptual questions. Prefer documentation over dependency source code.
2. Inspect the compact previews; only `doc_read` the ids that look relevant.
3. `symbol_lookup` for exact/near-exact symbol questions (paths, signatures,
   source spans).
4. Retrieve dependency implementation source only when documentation is
   insufficient, using the `source_path` and `source_span` the tools return.

If the index is missing or stale, run: `rust-knowledge index`

## Commands

```sh
cargo build                                   # build all crates
cargo +nightly clippy --all-targets --all-features   # lint (nightly required)
cargo test                                    # full suite (no nightly needed)
cargo test -p knowledge-index --test corpus    # one integration test file
cargo test -p knowledge-index --test corpus <name>   # one test by name
cargo run -p knowledge-cli -- index --manifest-path ./Cargo.toml   # build index
cargo run -p knowledge-cli -- eval evals/queries.toml              # retrieval eval
```

- **Nightly is required for clippy** (the workspace lint config uses nightly-only
  lints) and for generating rustdoc JSON. The test suite does **not** need
  nightly: integration tests run against a committed fixture workspace with
  prebuilt rustdoc artifacts.
- The index lands in `<workspace>/.rust-knowledge/` (gitignored). `--index-dir`
  overrides it. The `RUST_KNOWLEDGE_INDEX_DIR`, `RUST_KNOWLEDGE_CARGO`,
  `RUST_KNOWLEDGE_MANIFEST_PATH` and `RUST_KNOWLEDGE_LOG` (fallback
  `RUST_LOG`) env vars are figue's environment layer: each sits below its
  CLI flag, which always wins.

## Property-based and adversarial-input testing

`cargo test` runs two hostile-input suites alongside the example-based tests
(issue #5). The MCP surface accepts arbitrary strings from LLM clients; these
suites enforce that no such string can panic the server, hang it, corrupt
state, or produce unbounded output.

- **Adversarial corpora** — `crates/knowledge-index/tests/adversarial.rs` and
  `crates/knowledge-mcp/tests/adversarial.rs`: a committed, named list of
  hostile payloads (NUL bytes, control characters, unicode lookalikes, path
  traversal, query-syntax metacharacters, markdown fence bombs, ~1MB
  strings) run through every entry point an LLM client can reach —
  deterministically, no randomness, every run. **To add a payload**, append a
  `("name", string)` entry to `payloads()`; every test picks it up
  automatically.
- **Property suites** — `crates/knowledge-index/tests/properties.rs` and
  `crates/knowledge-mcp/tests/properties.rs`: proptest properties over
  arbitrary query text, symbols, ids, package/kind filters (including the
  no-filter-bypass invariant), limits, and markdown chunking. They prove
  invariants, **not retrieval quality** — `evals/queries.toml` stays the
  relevance gate; green property tests are not evidence of good ranking.

Reproducing and debugging a failure:

- proptest persists failing cases to `proptest-regressions/*.txt` beside the
  test file's crate. **Commit those files** (never gitignore them): they are
  replayed automatically on every run until the case passes again.
- `PROPTEST_CASES=<n> cargo test --test properties` runs a heavier sweep
  (proptest reads this variable natively). The committed default is 32 cases
  per property (16 through the MCP transport) to keep the suite fast.
- `PROPTEST_SEED=<hex> cargo test --test properties` reproduces one exact
  random sequence; proptest prints the seed of every failing run.
- Re-run a single property with
  `cargo test -p knowledge-index --test properties <name>`.

## Architecture

Four crates, one data flow. `knowledge-core` is the only crate the others all
depend on; it deliberately has **no** cargo/rustdoc/tantivy/MCP dependency so a
future vector retriever can plug in behind the same interface.

```
cargo metadata ──> CargoUniverse (exact PackageId identities, resolved graph)   knowledge-index::cargo
rustdoc JSON ────> documented API items (symbol paths, signatures, spans)        knowledge-index::rustdoc
README/docs ────> heading-structured chunks                                      knowledge-index::markdown
        │
        v
normalized KnowledgeDocument corpus (deterministic DocumentId)                  knowledge-index::corpus
        v
local Tantivy index (weighted fields, filters)                                  knowledge-index::tantivy_index
        v
KnowledgeRetriever trait (sync)                                                 knowledge-core::query
   ├── knowledge-cli  (bin `rust-knowledge`; figue over facet shapes)
   └── knowledge-mcp (bin `knowledge-mcp`; rmcp; thin adapter, no engine deps)
```

- **`knowledge-core`**: data model (`KnowledgeDocument`, `PackageIdentity`,
  `SourceKind`), `DocumentId`, `SearchQuery`/`SearchHit`, the `KnowledgeRetriever`
  trait. No engine dependencies.
- **`knowledge-index`**: ingestion + retrieval. One small module per concern
  (`cargo`, `config`, `error`, `rustdoc`, `markdown`, `corpus`,
  `tantivy_index`, `store`, `pipeline`, `eval`). `pipeline::index_workspace` is the
  end-to-end entry point used by both CLI and tests. `config` holds the
  figue/facet `WorkspaceConfig` root shared by both frontends.
- **`knowledge-cli` / `knowledge-mcp`**: thin frontends over the same engine.

### Key design decisions (see `docs/design.md` for full detail)

- **Identity is `PackageId`, never crate name.** Two versions of one crate are
  distinct identities. `DocumentId` is a deterministic SHA-256 over a canonical
  identity string — stable across rebuilds, independent of insertion order.
- **rustdoc JSON is nightly-only and versioned** (`format_version` 61). The
  parser checks the version and fails with a diagnostic on mismatch; bump
  `rustdoc-types` in lockstep with upstream format changes. `Crate` has no
  `crate_name` field; item kind is the tag of `Item.inner`, not a JSON field.
  Re-exports (`pub use`) are the primary API surface of re-export-heavy crates —
  the walker follows them and indexes targets under the re-export path.
- **Search is weighted lexical retrieval** (Tantivy). `symbol_path` is boosted
  highest; identifier-looking query tokens get extra term-query boosts so exact
  symbol queries dominate. The `body` field uses the English stemmer; identifier
  fields stay unstemmed. `symbol_lookup` is a separate exact→last-segment→
  conjunction path, not generic search.
- **`evals/queries.toml` is the regression harness** for any retrieval change.
  The integration test asserts every case passes (21/21, MRR ≈ 0.87). If you
  change tokenization, boosts, or chunking, run the eval.

## Conventions and gotchas

- **Lint config mandates `#[expect(..., reason = "...")]` over `#[allow]`**
  (`allow_attributes` / `allow_attributes_without_reason` are warn). Use
  `#[expect(clippy::lint, reason = "...")]` for deliberate suppressions. Note
  `#[expect]` warns if the lint doesn't actually fire — for a public item in a
  library crate, `dead_code` never fires, so don't add an expectation for it.
- **`clippy.toml` relaxes indexing/panic/unwrap/expect in tests, but only for
  unit tests (`#[cfg(test)]`), not integration tests in `tests/`.** Integration
  tests must still avoid `unwrap()`, `panic!`, and slicing/indexing — use
  `.expect(...)`, `.get(...)`, `.first()`.
- **The fixture workspace is its own cargo workspace**, excluded from the root
  (`fixtures/demo-workspace`). It deliberately exercises member-vs-path-
  dependency distinction and two versions of `base64` in one graph. Its rustdoc
  artifacts are committed under `fixtures/demo-workspace/prebuilt-rustdoc/` so
  tests never invoke nightly rustdoc.
- The MCP server is wired into Claude Code via `.mcp.json` and
  `.claude/settings.local.json` (points at `target/release/knowledge-mcp`).
- `tests/universe.rs` parses a committed `cargo metadata` blob whose absolute
  paths come from the machine that recorded it, so tests there must never touch
  the filesystem; they assert derivations (e.g. an identity carries over the
  manifest path cargo reported, and its root is the manifest's parent). The
  on-disk locatability sweep over all packages lives in `tests/fixture.rs`
  (`every_identity_is_locatable`), which runs against live `cargo metadata`.
