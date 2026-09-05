# Observability: logging, levels, and the telemetry-export seam

Both binaries (`rust-knowledge` and `knowledge-mcp`) initialize tracing
through one shared, layered helper: [`knowledge_index::telemetry`]. The
stack is `Registry` → `EnvFilter` → optional extra layers → a formatting
layer, and **every log line is written to stderr**. stdout is a data channel
in both binaries — JSON-RPC for the MCP server, `--json` output for the
CLI — and it stays pristine by construction: the writer choice makes log
output on stdout impossible, not discipline. Two integration tests pin it
(`knowledge-mcp/tests/logging.rs`, `knowledge-cli/tests/logging.rs`).

## Running with logging

```sh
# Default (info): counts and timings, no user query text.
rust-knowledge search "tokio spawn_blocking" --index-dir ~/.rust-knowledge

# More detail: -v bumps the built-in default from info to debug
# (query text appears in request span fields).
rust-knowledge -v search "tokio spawn_blocking"

# JSON logs, the export-ready encoding (one JSON object per line).
RUST_KNOWLEDGE_LOG_FORMAT=json rust-knowledge search spawn_blocking --json

# Full control via the standard filter syntax. Engine events come from
# knowledge_index::* targets (the serving line is knowledge_mcp).
RUST_KNOWLEDGE_LOG='info,knowledge_index=debug' knowledge-mcp
RUST_LOG=trace RUST_KNOWLEDGE_LOG_FORMAT=json knowledge-mcp
```

## Environment variables

| Variable | Meaning | Default when unset |
|---|---|---|
| `RUST_KNOWLEDGE_LOG` | Filter directive (kept for compatibility; highest priority) | — |
| `RUST_LOG` | Standard `tracing` filter directive | — |
| `RUST_KNOWLEDGE_LOG_FORMAT` | Log encoding: `human` or `json` (case-insensitive) | `human` |

Rules, in order:

1. **Precedence**: `RUST_KNOWLEDGE_LOG` → `RUST_LOG` → the built-in
   default. The first variable that is set and non-empty wins; nothing is
   merged. An empty or whitespace-only value counts as unset.
2. **Built-in default**: `info,tantivy=warn,rmcp=warn` — overall info,
   with per-target levels for chatty libraries. The CLI's `-v` flag bumps
   the default to `debug,tantivy=warn,rmcp=warn`; an explicit env var
   always wins over `-v`.
3. **Malformed directives never abort startup**: a directive that fails to
   parse degrades to the built-in default and a warning is logged through
   the fallback subscriber (so the binary still starts and serves).
4. **Unknown format values degrade to human** with a warning.
5. `log`-crate records (dependencies that do not use `tracing`, e.g.
   tantivy) are bridged into the same stack and filtered by the same
   directives — `tantivy=warn` in the default keeps tantivy's per-file
   DEBUG records out of normal output.

The per-target levels are empirical (measured on this dependency graph):
`rmcp` logs lifecycle chatter (service initialized, stream terminated,
serve finished) at `info`, and `tantivy` emits per-read `DEBUG` records
through the `log` crate; the default suppresses both while keeping the
engine's own stage/request lines visible.

## Log formats

**human** (default): compact, no target prefix, one line per event —
format unchanged from earlier releases; the destination moved from
stdout to stderr in this release, so capture logs with `2>` or `2>&1`.

```
2026-09-05T08:59:30.408409Z  INFO search done hits=1 elapsed_ms=1
```

**json** (`RUST_KNOWLEDGE_LOG_FORMAT=json`): the export-ready encoding —
one JSON object per line, ready for log shippers and for the OTLP path
below.

```json
{"timestamp":"2026-09-05T08:59:30.420103Z","level":"INFO",
 "fields":{"message":"search done","hits":1,"elapsed_ms":0},
 "target":"knowledge_index::tantivy_index::retriever",
 "span":{"query":"write_all","name":"search"},
 "spans":[{"query":"write_all","name":"search"}]}
```

Fields: `timestamp`, `level`, `target` (module path), `fields` (the
event's own fields, including `message`), `span` (the innermost span and
its fields), `spans` (the full span context chain). Span fields declared
empty and recorded later (e.g. `package_ingestion.documents`) appear in
`span`/`spans` once recorded.

## Level policy

What belongs at each level in this codebase:

| Level | Contents | Examples |
|---|---|---|
| `error` | Reserved for unrecoverable states in a running process. Failures are normally returned as typed `thiserror` errors and reported by the frontend (CLI `error:` line, MCP error result), not logged. | — |
| `warn` | Degraded but continued operation. | malformed filter directive fallback, skipped rustdoc generation, failed markdown file read |
| `info` | Stage boundaries and completion counts/timings. Never user text. | serving MCP (documents), built corpus (packages/documents/elapsed_ms), index built, search done (hits/elapsed_ms), symbol lookup done |
| `debug` | User text and internal decisions. | `search`/`symbol_lookup` span fields (query, symbol), `doc_get` span (id), lenient query-parse errors |
| `trace` | Everything else, including dependency internals via `RUST_LOG=trace`. | tantivy directory records, rmcp request/response plumbing |

**Content policy**: document bodies are never logged. User query text and
symbols appear **only at debug level** (span fields), so default `info`
output carries counts and timings without user input. Document ids are
content hashes and package names/paths are not user-authored, so both are
safe at info/debug.

## Span inventory

Indexing path (`rust-knowledge index`):

| Span | Level | Fields |
|---|---|---|
| `cargo_metadata` | info | — |
| `rustdoc_generation` | info | — |
| `rustdoc_normalize` | info | package |
| `package_ingestion` | info | packages, documents (recorded at completion) |
| `markdown_discovery` | info | package |
| `markdown_ingestion` | info | file |
| `index_build` | info | documents |

Request path (shared by both frontends, at the `KnowledgeRetriever` impl
boundary):

| Span | Level | Fields |
|---|---|---|
| `search` | debug | query |
| `symbol_lookup` | debug | symbol |
| `doc_get` | debug | id |

## Hot-path cost

Request spans run on every search/symbol lookup/get call. They are kept
lean on purpose: one span with one or two fields, one `info` completion
event with counts, and no per-hit events. A disabled span (debug under an
info filter) is a near-free no-op, and an enabled two-field span plus one
event is nanoseconds against a Tantivy query — but that is a claim to keep
honest: when a benchmark suite lands (criterion is already prewarmed in
the local registry cache), pin request-path overhead there so regressions
show up as numbers, not anecdotes.

## Telemetry-export seam (OpenTelemetry / OTLP)

Wiring an exporter today is deliberately out of scope; the seam is the
init API itself. [`telemetry::init_with`] accepts any additional layer
stacked between the filter and the formatting layer:

```rust
pub fn init_with<L>(options: &TelemetryOptions, extra: L) -> InitStatus
where
    L: tracing_subscriber::layer::Layer<
            tracing_subscriber::layer::Layered<EnvFilter, Registry>,
        > + Send + Sync + 'static,
```

That bound is exactly what `tracing-opentelemetry`'s layer implements: it
needs `LookupSpan`, which the `Registry` at the bottom of the stack
provides through the whole stack. Adding real OTLP export later is a
drop-in behind a cargo feature — no redesign, no new init path:

```rust
// Sketch of the future `otlp` cargo feature — NOT wired today.
let tracer = opentelemetry_otlp::new_pipeline()
    .tracing()
    .with_exporter(opentelemetry_otlp::new_exporter().tonic())
    .install_batch(opentelemetry_sdk::runtime::Tokio)?;
let layer = tracing_opentelemetry::layer().with_tracer(tracer);

// The existing seam, unchanged:
knowledge_index::telemetry::init_with(&TelemetryOptions::default(), layer)
```

The exporter layer stacks above `EnvFilter`, so when the `otlp`
feature lands, exports are gated by the same filter directives as
stderr output: they honor `RUST_KNOWLEDGE_LOG`/`RUST_LOG`, and events
the filter drops are never exported.

The feature owns the exporter lifecycle (flushing the provider on
shutdown); `init_with` returns an `InitStatus` describing the installed
filter/format so the caller can report where telemetry is going. Until
then, `RUST_KNOWLEDGE_LOG_FORMAT=json` is the export path: structured
JSON on stderr that any log shipper can pick up.

[`knowledge_index::telemetry`]: ../crates/knowledge-index/src/telemetry.rs
[`telemetry::init_with`]: ../crates/knowledge-index/src/telemetry.rs
