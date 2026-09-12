//! Shared, layered tracing initialization for both binaries
//! (`rust-knowledge` and `knowledge-mcp`).
//!
//! Both frontends call [init] (or [init_with] to stack an extra layer on
//! top), so log behavior is identical everywhere:
//!
//! * every log line goes to **stderr** — stdout is a data channel in both
//!   binaries (JSON-RPC for the MCP server, `--json` output for the CLI) and
//!   must stay pristine;
//! * the filter directive comes from [FILTER_ENV] (`RUST_KNOWLEDGE_LOG`,
//!   kept for compatibility), then [RUST_LOG_ENV] (`RUST_LOG`), then a
//!   built-in default with per-target levels for noisy libraries;
//! * [FORMAT_ENV] (`RUST_KNOWLEDGE_LOG_FORMAT=json`) switches to structured
//!   JSON logs — the export-ready encoding;
//! * malformed directives never abort startup: they degrade to the built-in
//!   default with a warn;
//! * a second init in the same process (tests) is tolerated, not a panic;
//! * `log`-crate records (dependencies that do not use `tracing`) are
//!   bridged into the same stack, filtered per target like everything
//!   else — the behavior the ad-hoc `SubscriberInitExt::init()` setups
//!   this module replaced used to provide.
//!
//! The stack is `Registry` → `EnvFilter` → extra layers → formatting layer,
//! so the init API is layer-agnostic: [init_with] accepts any additional
//! `tracing_subscriber::layer::Layer` whose inner subscriber is
//! `Layered<EnvFilter, Registry>`. That bound is exactly what a future
//! OpenTelemetry/OTLP export layer (`tracing-opentelemetry` behind a cargo
//! feature) implements — the registry at the bottom of the stack provides
//! `LookupSpan`, which is all such layers need. Wiring the exporter is
//! deliberately out of scope today; the seam is documented in
//! docs/observability.md.

use std::io;

use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Layer, Layered, SubscriberExt};
use tracing_subscriber::registry::Registry;

/// Explicit filter override, kept for compatibility with earlier releases.
pub const FILTER_ENV: &str = "RUST_KNOWLEDGE_LOG";
/// Standard `tracing` filter variable, honored when [FILTER_ENV] is unset.
pub const RUST_LOG_ENV: &str = "RUST_LOG";
/// Selects the log encoding: `human` (default) or `json`.
pub const FORMAT_ENV: &str = "RUST_KNOWLEDGE_LOG_FORMAT";

/// Built-in default filter: info overall, noisy libraries toned down.
pub const DEFAULT_FILTER: &str = "info,tantivy=warn,rmcp=warn";
/// Built-in default filter with `-v`: debug overall, noisy libraries still
/// toned down.
pub const DEFAULT_FILTER_VERBOSE: &str = "debug,tantivy=warn,rmcp=warn";

/// The built-in default filter for the given verbosity.
pub const fn default_filter(verbose: bool) -> &'static str {
    if verbose {
        DEFAULT_FILTER_VERBOSE
    } else {
        DEFAULT_FILTER
    }
}

/// Log encoding, selected by [FORMAT_ENV].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Compact human-readable lines — the format both binaries always used.
    #[default]
    Human,
    /// Structured JSON, one object per line — the export-ready encoding
    /// consumed by log shippers, and the shape a future OTLP exporter reads
    /// from.
    Json,
}

/// Options the frontends control; everything else comes from the
/// environment so both binaries behave identically.
#[derive(Clone, Copy, Debug, Default)]
pub struct TelemetryOptions {
    /// Bump the built-in default filter from info to debug. Ignored when
    /// [FILTER_ENV] or [RUST_LOG_ENV] is set — explicit always wins.
    pub verbose: bool,
}

/// The zero member of the layer extension point: implements `Layer` for
/// any subscriber and does nothing. [init] stacks it where [init_with]
/// accepts a real layer.
pub struct NoLayer;

impl<S> Layer<S> for NoLayer where S: Subscriber {}

/// Where the effective filter directive came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterSource {
    /// `RUST_KNOWLEDGE_LOG` (explicit override).
    RustKnowledgeLog,
    /// `RUST_LOG`.
    RustLog,
    /// The built-in default (optionally bumped by `TelemetryOptions::verbose`).
    Default,
}

/// A filter directive that failed to parse. Startup continues on the
/// built-in default and a warn is emitted through the installed subscriber.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MalformedDirective {
    /// The environment variable the directive came from.
    pub env: &'static str,
    /// The value that failed to parse.
    pub value: String,
    /// The parser error.
    pub error: String,
}

/// What [init] did — for tests and callers that care to check. The return
/// value is informational; binaries ignore it.
#[derive(Clone, Debug)]
pub struct InitStatus {
    /// `false` when a global subscriber was already installed (e.g. another
    /// test initialized first); the existing subscriber stays in place.
    pub installed: bool,
    /// The filter directive actually in effect.
    pub filter_directive: String,
    /// Where `InitStatus::filter_directive` came from.
    pub filter_source: FilterSource,
    /// The log encoding in effect.
    pub format: LogFormat,
    /// Set when an environment directive failed to parse and the default
    /// was used instead.
    pub malformed_directive: Option<MalformedDirective>,
    /// Set when [FORMAT_ENV] held an unrecognized value and human output
    /// was used instead.
    pub unknown_format: Option<String>,
}

/// Filter-precedence resolution, pure so it can be unit-tested without
/// touching the global subscriber: [FILTER_ENV] → [RUST_LOG_ENV] → built-in
/// default (bumped by `verbose`). An empty or whitespace-only value counts
/// as unset. A directive that fails to parse falls back to the default and
/// reports the failure.
fn resolve_filter(
    explicit: Option<&str>,
    rust_log: Option<&str>,
    verbose: bool,
) -> ResolvedFilter {
    for (env, raw, source) in [
        (FILTER_ENV, explicit, FilterSource::RustKnowledgeLog),
        (RUST_LOG_ENV, rust_log, FilterSource::RustLog),
    ] {
        let Some(directive) = raw.map(str::trim).filter(|d| !d.is_empty()) else {
            continue;
        };
        return match EnvFilter::try_new(directive) {
            Ok(_) => ResolvedFilter {
                directive: directive.to_string(),
                source,
                malformed: None,
            },
            Err(error) => ResolvedFilter {
                directive: default_filter(verbose).to_string(),
                source: FilterSource::Default,
                malformed: Some(MalformedDirective {
                    env,
                    value: directive.to_string(),
                    error: error.to_string(),
                }),
            },
        };
    }
    ResolvedFilter {
        directive: default_filter(verbose).to_string(),
        source: FilterSource::Default,
        malformed: None,
    }
}

#[derive(Clone, Debug)]
struct ResolvedFilter {
    directive: String,
    source: FilterSource,
    malformed: Option<MalformedDirective>,
}

/// Format resolution: `json` selects [LogFormat::Json], `human` (or unset)
/// selects [LogFormat::Human]; anything else degrades to human and the
/// unrecognized value is reported for a warn.
fn resolve_format(raw: Option<&str>) -> (LogFormat, Option<String>) {
    let Some(value) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return (LogFormat::Human, None);
    };
    match value.to_ascii_lowercase().as_str() {
        "human" => (LogFormat::Human, None),
        "json" => (LogFormat::Json, None),
        _ => (LogFormat::Human, Some(value.to_string())),
    }
}

/// Reads an environment variable, treating empty or whitespace-only values
/// as unset.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Initializes the shared layered subscriber for a binary: `Registry` +
/// `EnvFilter` + a formatting layer, all writing to stderr.
///
/// The filter comes from [FILTER_ENV] → [RUST_LOG_ENV] → the built-in
/// default (with `options.verbose` bumping info to debug); the encoding
/// from [FORMAT_ENV]. Malformed directives degrade to the default with a
/// warn instead of aborting. Safe to call twice: the second call is a
/// no-op, not a panic.
pub fn init(options: &TelemetryOptions) -> InitStatus {
    init_with(options, NoLayer)
}

/// Like [init], plus an extra layer stacked between the filter and the
/// formatting layer. This is the telemetry-export seam: a future
/// OpenTelemetry/OTLP layer (tracing-opentelemetry behind a cargo feature)
/// implements `Layer<Layered<EnvFilter, Registry>>` and slots in here with
/// no other change — see docs/observability.md.
pub fn init_with<L>(options: &TelemetryOptions, extra: L) -> InitStatus
where
    L: Layer<Layered<EnvFilter, Registry>> + Send + Sync + 'static,
{
    try_init(
        options,
        env_nonempty(FILTER_ENV).as_deref(),
        env_nonempty(RUST_LOG_ENV).as_deref(),
        env_nonempty(FORMAT_ENV).as_deref(),
        io::stderr,
        extra,
    )
}

/// The one install path. Private so tests can inject a writer and explicit
/// env values; the public API always reads the environment and writes to
/// stderr.
fn try_init<W, L>(
    options: &TelemetryOptions,
    explicit: Option<&str>,
    rust_log: Option<&str>,
    format: Option<&str>,
    writer: W,
    extra: L,
) -> InitStatus
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    L: Layer<Layered<EnvFilter, Registry>> + Send + Sync + 'static,
{
    let resolved = resolve_filter(explicit, rust_log, options.verbose);
    let (format, unknown_format) = resolve_format(format);
    let filter = EnvFilter::new(&resolved.directive);

    let installed = match format {
        LogFormat::Json => install_stack(
            filter,
            extra,
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(writer),
        )
        .is_ok(),
        LogFormat::Human => install_stack(
            filter,
            extra,
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .compact()
                .with_writer(writer),
        )
        .is_ok(),
    };

    // Bridge `log`-crate records into the subscriber — the behavior the
    // ad-hoc `SubscriberInitExt::init()` setups this module replaced used
    // to provide, so dependencies without a `tracing` dependency still
    // reach the stack (EnvFilter keeps them toned down per target). Like
    // the subscriber itself this is once-only per process: an already-set
    // logger (an earlier init, e.g. in tests) is tolerated.
    if tracing_log::LogTracer::init().is_err() {
        tracing::debug!("log crate bridge already initialized");
    }

    // Warnings can only be emitted through a live subscriber, so they come
    // after the install attempt and reach whichever subscriber is active
    // (including one installed by an earlier test).
    if let Some(malformed) = &resolved.malformed {
        tracing::warn!(
            env = malformed.env,
            value = %malformed.value,
            error = %malformed.error,
            fallback = %resolved.directive,
            "malformed log filter directive; using built-in default"
        );
    }
    if let Some(value) = &unknown_format {
        tracing::warn!(
            env = FORMAT_ENV,
            value = %value,
            "unknown log format; using human-readable output"
        );
    }

    InitStatus {
        installed,
        filter_directive: resolved.directive,
        filter_source: resolved.source,
        format,
        malformed_directive: resolved.malformed,
        unknown_format,
    }
}

/// Builds `Registry` → `EnvFilter` → extra layer → formatting layer and
/// installs it as the global subscriber, tolerating an already-set one.
fn install_stack<L, F>(
    filter: EnvFilter,
    extra: L,
    fmt_layer: F,
) -> Result<(), tracing::subscriber::SetGlobalDefaultError>
where
    L: Layer<Layered<EnvFilter, Registry>> + Send + Sync + 'static,
    F: Layer<Layered<L, Layered<EnvFilter, Registry>>> + Send + Sync + 'static,
{
    let stack = tracing_subscriber::registry()
        .with(filter)
        .with(extra)
        .with(fmt_layer);
    tracing::subscriber::set_global_default(stack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn explicit_filter_wins_over_rust_log() {
        let r = resolve_filter(Some("debug"), Some("warn"), false);
        assert_eq!(r.directive, "debug");
        assert_eq!(r.source, FilterSource::RustKnowledgeLog);
        assert!(r.malformed.is_none());
    }

    #[test]
    fn rust_log_wins_over_default() {
        let r = resolve_filter(None, Some("tantivy=debug"), false);
        assert_eq!(r.directive, "tantivy=debug");
        assert_eq!(r.source, FilterSource::RustLog);
    }

    #[test]
    fn empty_explicit_falls_through_to_rust_log() {
        for empty in ["", "   	"] {
            let r = resolve_filter(Some(empty), Some("warn"), false);
            assert_eq!(r.source, FilterSource::RustLog, "explicit {empty:?}");
        }
    }

    #[test]
    fn default_filter_depends_on_verbose() {
        let r = resolve_filter(None, None, false);
        assert_eq!(r.directive, DEFAULT_FILTER);
        assert_eq!(r.source, FilterSource::Default);

        let r = resolve_filter(None, None, true);
        assert_eq!(r.directive, DEFAULT_FILTER_VERBOSE);
    }

    #[test]
    fn malformed_explicit_falls_back_to_default() {
        let r = resolve_filter(Some("info=bogus"), Some("debug"), true);
        // The fallback is the default, not the lower-priority RUST_LOG:
        // a broken override must not silently re-enable a level the
        // operator did not ask for.
        assert_eq!(r.directive, DEFAULT_FILTER_VERBOSE);
        assert_eq!(r.source, FilterSource::Default);
        let malformed = r.malformed.expect("malformed metadata");
        assert_eq!(malformed.env, FILTER_ENV);
        assert_eq!(malformed.value, "info=bogus");
        assert!(!malformed.error.is_empty());
    }

    #[test]
    fn malformed_rust_log_falls_back_to_default() {
        let r = resolve_filter(None, Some("not=level!!"), false);
        assert_eq!(r.directive, DEFAULT_FILTER);
        assert_eq!(r.source, FilterSource::Default);
        let malformed = r.malformed.expect("malformed metadata");
        assert_eq!(malformed.env, RUST_LOG_ENV);
    }

    #[test]
    fn verbose_is_ignored_when_a_directive_is_set() {
        for (explicit, rust_log) in [(Some("info"), None), (None, Some("info"))] {
            let r = resolve_filter(explicit, rust_log, true);
            assert_eq!(r.directive, "info", "verbose must not override env");
        }
    }

    #[test]
    fn format_parsing() {
        assert_eq!(resolve_format(None), (LogFormat::Human, None));
        assert_eq!(resolve_format(Some("")), (LogFormat::Human, None));
        assert_eq!(resolve_format(Some("human")), (LogFormat::Human, None));
        assert_eq!(resolve_format(Some(" JSON ")), (LogFormat::Json, None));
        let (format, unknown) = resolve_format(Some("xml"));
        assert_eq!(format, LogFormat::Human);
        assert_eq!(unknown.as_deref(), Some("xml"));
    }

    /// A writer that records every log byte in shared memory, so the JSON
    /// layer can be asserted on without a process or a pipe.
    #[derive(Clone, Default)]
    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            // A log writer must never panic the process: a poisoned lock
            // (some other test panicked mid-write) drops the bytes instead.
            let mut target = self
                .0
                .lock()
                .map_err(|_poisoned| io::Error::other("log writer lock poisoned"))?;
            target.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedLogWriter {
        type Writer = SharedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The single test in this binary that installs the global subscriber
    /// (init is once per process). Proves the JSON layer emits structured
    /// span and event fields, and that a second init is tolerated.
    #[test]
    fn json_mode_exposes_span_and_event_fields_and_tolerates_reinit() {
        let writer = SharedLogWriter::default();
        let status = try_init(
            &TelemetryOptions::default(),
            None,
            Some("trace"),
            Some("json"),
            writer.clone(),
            NoLayer,
        );
        assert!(status.installed);
        assert_eq!(status.filter_directive, "trace");
        assert_eq!(status.format, LogFormat::Json);

        // Indexing-stage shape: an info span whose fields ride along events.
        let span = tracing::info_span!("index_build", documents = 42);
        let enter = span.enter();
        tracing::info!(packages = 4, "built corpus");
        drop(enter);

        // Request-stage shape: user text at debug, counts/timings at info.
        let request = tracing::debug_span!("search", query = "writer::flush");
        let enter = request.enter();
        tracing::info!(hits = 3, elapsed_ms = 12, "search done");
        drop(enter);

        // `log`-crate records (dependencies without `tracing`) are bridged
        // into the same stack.
        log::info!("bridged log record");

        let buf = writer
            .0
            .lock()
            .expect("log writer lock")
            .clone();
        let text = String::from_utf8(buf).expect("utf-8 log output");
        assert!(!text.is_empty(), "expected JSON log lines");

        let mut saw_corpus = false;
        let mut saw_search = false;
        let mut saw_bridge = false;
        for line in text.lines() {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("every JSON-mode line is JSON");
            let message = value
                .get("fields")
                .and_then(|f| f.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if message == "built corpus" {
                saw_corpus = true;
                assert_eq!(
                    value
                        .get("fields")
                        .and_then(|f| f.get("packages"))
                        .and_then(serde_json::Value::as_u64),
                    Some(4)
                );
                assert_eq!(
                    value
                        .get("span")
                        .and_then(|s| s.get("documents"))
                        .and_then(serde_json::Value::as_u64),
                    Some(42)
                );
            }
            if message == "bridged log record" {
                saw_bridge = true;
            }
            if message == "search done" {
                saw_search = true;
                let fields = value.get("fields").expect("fields object");
                assert_eq!(
                    fields.get("hits").and_then(serde_json::Value::as_u64),
                    Some(3)
                );
                assert_eq!(
                    fields
                        .get("elapsed_ms")
                        .and_then(serde_json::Value::as_u64),
                    Some(12)
                );
                let span = value.get("span").expect("span object");
                assert_eq!(
                    span.get("name").and_then(serde_json::Value::as_str),
                    Some("search")
                );
                assert_eq!(
                    span.get("query").and_then(serde_json::Value::as_str),
                    Some("writer::flush")
                );
            }
        }
        assert!(saw_corpus, "indexing event missing from JSON output");
        assert!(saw_search, "request event missing from JSON output");
        assert!(saw_bridge, "log-crate record missing from JSON output");

        // A second init in the same process (tests share one) must be
        // tolerated: no panic, the existing subscriber stays in place.
        let again = try_init(
            &TelemetryOptions { verbose: true },
            None,
            None,
            Some("json"),
            writer.clone(),
            NoLayer,
        );
        assert!(!again.installed);
    }
}
