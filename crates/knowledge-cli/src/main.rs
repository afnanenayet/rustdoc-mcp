//! CLI entry point for the `rust-knowledge` binary.
//!
//! The argument surface follows the figue recipes (issue #2): a flattened
//! figue config root over [`WorkspaceConfig`] layers CLI flags over the
//! `RUST_KNOWLEDGE`_* environment variables over defaults with figue's own
//! precedence; [`figue::FigueBuiltins`] contributes --help/--version and
//! friends; subcommands come from `#[facet(args::subcommand)]`; and
//! help/version/diagnostics with their exit codes are figue's own
//! (`DriverOutcome::unwrap`), not emulated. The generated HTML reference
//! page (`rust-knowledge config-docs`) is figue's `generate_html_help`
//! output for the [Cli] shape.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use facet::Facet;
use figue::{self as args, FigueBuiltins};
use knowledge_core::KnowledgeRetriever;
use knowledge_index::CargoUniverse;
use knowledge_index::config::{WorkspaceConfig, parse_std_args};
use knowledge_index::corpus::{CorpusOptions, RustdocScope, build_corpus};
use knowledge_index::rustdoc::{GeneratedRustdocProvider, PrebuiltRustdocProvider};
use knowledge_index::telemetry::TelemetryOptions;

const PROGRAM: &str = "rust-knowledge";
const ABOUT: &str = "Search documentation of the resolved Cargo dependency universe of a workspace";

/// Command-line surface of `rust-knowledge`.
#[derive(Facet, Debug)]
struct Cli {
    /// Workspace knobs, layered by figue: flags beat $`RUST_KNOWLEDGE`_* env
    /// vars, which beat defaults.
    #[facet(args::config, args::env_prefix = "RUST_KNOWLEDGE", flatten)]
    config: WorkspaceConfig,

    /// Verbose logging (forces the "debug" filter).
    #[facet(args::named, args::short = 'v', default)]
    verbose: bool,

    #[facet(flatten)]
    builtins: FigueBuiltins,

    #[facet(args::subcommand)]
    command: Command,
}

#[derive(Facet, Debug)]
#[repr(u8)]
enum Command {
    /// List the packages in the resolved Cargo dependency universe.
    Packages {
        /// Emit JSON (one object per line).
        #[facet(args::named, default)]
        json: bool,
    },

    /// Build the normalized documentation corpus and print it (debug view).
    DumpDocs {
        /// Only documents of this package (name or name@version).
        #[facet(args::named)]
        package: Option<String>,

        /// Which packages get rustdoc JSON generated.
        #[facet(args::named, default = "workspace")]
        rustdoc_scope: String,

        /// Read prebuilt rustdoc JSON artifacts from this directory instead
        /// of invoking cargo.
        #[facet(args::named)]
        prebuilt_rustdoc: Option<PathBuf>,

        /// Emit one JSON document per line.
        #[facet(args::named, default)]
        json: bool,
    },

    /// Build the persistent knowledge index for the workspace.
    Index {
        /// Which packages get rustdoc JSON generated.
        #[facet(args::named, default = "workspace")]
        rustdoc_scope: String,

        /// Toolchain used for rustdoc generation (default: nightly).
        #[facet(args::named)]
        toolchain: Option<String>,

        /// Read prebuilt rustdoc JSON artifacts from this directory instead
        /// of invoking cargo.
        #[facet(args::named)]
        prebuilt_rustdoc: Option<PathBuf>,
    },

    /// Search the knowledge index.
    Search {
        /// Free-form query: natural language or Rust identifiers.
        #[facet(args::positional)]
        query: String,

        /// Restrict to these packages (name or name@version). Repeatable.
        // NOTE: the field is named in the singular on purpose: figue's
        // scalar-to-list coercion looks fields up by their Rust name, so a
        // `rename`d Vec field fails to deserialize single-occurrence flags
        // (figue 4.0.5). The singular name kebab-cases to `--package`
        // without a rename.
        #[facet(args::named, default)]
        package: Vec<String>,

        /// Restrict to source kinds (`rustdoc_item`, `rustdoc_module`,
        /// `crate_readme`, `markdown_document`). Repeatable.
        #[facet(args::named, default)]
        source_kind: Vec<String>,

        /// Restrict to item kinds (function, struct, trait, ...). Repeatable.
        #[facet(args::named, default)]
        item_kind: Vec<String>,

        /// Maximum number of results.
        #[facet(args::named, default = 8)]
        limit: usize,

        /// Emit JSON (one hit per line).
        #[facet(args::named, default)]
        json: bool,
    },

    /// Retrieve one document by its stable id.
    Get {
        /// Document id (as printed by search).
        #[facet(args::positional)]
        id: String,

        /// Emit the full JSON document.
        #[facet(args::named, default)]
        json: bool,
    },

    /// Look up a symbol by (partial) path.
    Symbol {
        /// Symbol path or last segment, e.g. `demo_core::writer::Writer::flush`
        /// or `spawn_blocking`.
        #[facet(args::positional)]
        symbol: String,

        /// Restrict to these packages (name or name@version). Repeatable.
        #[facet(args::named, default)]
        package: Vec<String>,

        /// Emit JSON (one entry per line).
        #[facet(args::named, default)]
        json: bool,
    },

    /// Run the retrieval evaluation set against the knowledge index.
    Eval {
        /// Path to the eval file (TOML, [[case]] entries).
        #[facet(args::positional)]
        file: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = parse_std_args::<Cli>(PROGRAM, env!("CARGO_PKG_VERSION"), ABOUT).unwrap();
    // Shared layered tracing init: logs go to stderr (stdout carries the
    // data output of --json modes); -v bumps the built-in default filter,
    // RUST_KNOWLEDGE_LOG / RUST_LOG still win over it.
    knowledge_index::telemetry::init(&TelemetryOptions {
        verbose: cli.verbose,
    });

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Packages { json } => packages(cli, *json),
        Command::DumpDocs {
            package,
            rustdoc_scope,
            prebuilt_rustdoc,
            json,
        } => dump_docs(
            cli,
            package.as_ref(),
            rustdoc_scope,
            prebuilt_rustdoc.as_ref(),
            *json,
        ),
        Command::Index {
            rustdoc_scope,
            toolchain,
            prebuilt_rustdoc,
        } => index(
            cli,
            rustdoc_scope,
            toolchain.as_ref(),
            prebuilt_rustdoc.as_ref(),
        ),
        Command::Search {
            query,
            package,
            source_kind,
            item_kind,
            limit,
            json,
        } => search(cli, query, package, source_kind, item_kind, *limit, *json),
        Command::Get { id, json } => get(cli, id, *json),
        Command::Symbol {
            symbol,
            package,
            json,
        } => symbol_lookup(cli, symbol, package, *json),
        Command::Eval { file } => eval(cli, file),
    }
}

fn load_universe(cli: &Cli) -> anyhow::Result<CargoUniverse> {
    CargoUniverse::load_with(
        cli.config.manifest_path.as_deref(),
        cli.config.cargo.as_deref(),
    )
    .context("failed to load the resolved Cargo universe")
}

fn packages(cli: &Cli, json: bool) -> anyhow::Result<()> {
    let universe = load_universe(cli)?;

    let mut rows: Vec<_> = universe
        .packages()
        .map(|p| {
            let identity = universe.identity(p);
            (
                identity.name.clone(),
                identity.version.clone(),
                universe.origin(p).as_str().to_string(),
                universe.enabled_features(&p.id).join(","),
                identity.manifest_path.clone(),
                identity.package_id.clone(),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    if json {
        for (name, version, origin, features, manifest, package_id) in rows {
            println!(
                "{}",
                serde_json::json!({
                    "name": name,
                    "version": version,
                    "origin": origin,
                    "package_id": package_id,
                    "manifest_path": manifest,
                    "enabled_features": features,
                })
            );
        }
        return Ok(());
    }

    println!(
        "{:<24} {:<12} {:<10} {:<28} PATH",
        "PACKAGE", "VERSION", "ORIGIN", "FEATURES"
    );
    for (name, version, origin, features, manifest, _) in &rows {
        println!(
            "{:<24} {:<12} {:<10} {:<28} {}",
            name,
            version,
            origin,
            if features.is_empty() { "-" } else { features },
            manifest.display()
        );
    }
    Ok(())
}

fn parse_scope(name: &str) -> anyhow::Result<RustdocScope> {
    RustdocScope::parse(name).ok_or_else(|| {
        anyhow::anyhow!("unknown rustdoc scope {name:?}: use workspace, all or none")
    })
}

fn dump_docs(
    cli: &Cli,
    package: Option<&String>,
    rustdoc_scope: &str,
    prebuilt_rustdoc: Option<&PathBuf>,
    json: bool,
) -> anyhow::Result<()> {
    let universe = load_universe(cli)?;
    let scope = parse_scope(rustdoc_scope)?;
    let index_dir = cli.config.resolve_index_dir(universe.workspace_root());

    let provider: Box<dyn knowledge_index::rustdoc::RustdocProvider> = match prebuilt_rustdoc {
        Some(dir) => Box::new(PrebuiltRustdocProvider { dir: dir.clone() }),
        None => Box::new(GeneratedRustdocProvider::new(
            &universe,
            index_dir.join("cache").join("rustdoc"),
            Some("nightly".to_string()),
            cli.config.cargo.as_deref(),
        )),
    };

    let options = CorpusOptions {
        rustdoc_scope: scope,
    };
    let (documents, report) = build_corpus(&universe, provider.as_ref(), &options)
        .context("failed to build the documentation corpus")?;

    eprintln!(
        "# corpus: {} packages, {} rustdoc generated, {} markdown files, {} documents",
        report.packages,
        report.rustdoc_packages,
        report.markdown_files,
        documents.len()
    );
    for (spec, reason) in &report.skipped {
        eprintln!("# skipped {spec}: {reason}");
    }
    for warning in &report.warnings {
        eprintln!("# warning: {warning}");
    }

    let documents: Vec<_> = match package {
        None => documents,
        Some(spec) => {
            let pkg = universe.resolve_spec(spec).map_err(anyhow::Error::from)?;
            let id = &pkg.id.repr;
            documents
                .into_iter()
                .filter(|d| d.package.package_id == *id)
                .collect()
        }
    };

    for doc in &documents {
        if json {
            println!("{}", serde_json::to_string(doc)?);
            continue;
        }
        println!("--------------------------------------------------");
        println!("id: {}", doc.id);
        println!("from: {}", doc.provenance());
        println!("context: {}", doc.context());
        if let Some(sig) = &doc.signature {
            println!("signature: {sig}");
        }
        if let Some(span) = &doc.source_span
            && let Some(path) = &doc.source_path
        {
            println!(
                "source: {}:{}-{}",
                path.display(),
                span.start_line,
                span.end_line
            );
        }
        if !doc.related_symbols.is_empty() {
            println!("related: {}", doc.related_symbols.join(", "));
        }
        let text = if doc.text.len() > 1200 {
            let end = doc.text.floor_char_boundary(1200);
            format!("{}...", doc.text.get(..end).unwrap_or_default().trim_end())
        } else {
            doc.text.clone()
        };
        println!("{text}");
    }

    if documents.is_empty() && package.is_some() {
        eprintln!("# no documents matched");
    }
    Ok(())
}

fn index(
    cli: &Cli,
    rustdoc_scope: &str,
    toolchain: Option<&String>,
    prebuilt_rustdoc: Option<&PathBuf>,
) -> anyhow::Result<()> {
    let scope = parse_scope(rustdoc_scope)?;
    let options = knowledge_index::IndexOptions {
        rustdoc_scope: scope,
        toolchain: Some(toolchain.cloned().unwrap_or_else(|| "nightly".to_string())),
        prebuilt_rustdoc: prebuilt_rustdoc.cloned(),
        skip_rustdoc: false,
        cargo: cli.config.cargo.clone(),
    };
    let outcome = knowledge_index::index_workspace(
        cli.config.manifest_path.as_deref(),
        cli.config.index_dir.as_deref(),
        &options,
    )
    .context("indexing failed")?;

    println!(
        "indexed {} packages into {} ({} documents)",
        outcome.corpus.packages,
        outcome.index_dir.display(),
        outcome.meta.document_count
    );
    for (spec, reason) in &outcome.meta.skipped {
        eprintln!("skipped {spec}: {reason}");
    }
    for warning in &outcome.meta.warnings {
        eprintln!("warning: {warning}");
    }
    println!("search it with: rust-knowledge search <query>");
    Ok(())
}

fn parse_source_kinds(raw: &[String]) -> anyhow::Result<Vec<knowledge_core::SourceKind>> {
    raw.iter()
        .map(|s| {
            s.parse::<knowledge_core::SourceKind>()
                .map_err(anyhow::Error::msg)
        })
        .collect()
}

/// Opens the index for the search/get/symbol/eval commands. When
/// `--index-dir` is absent, a cargo metadata run discovers the
/// workspace — with the resolved `--cargo` binary, so the flag is
/// honored on this spawn too.
fn open_retriever(cli: &Cli) -> anyhow::Result<knowledge_index::TantivyRetriever> {
    knowledge_index::open_retriever_with(
        cli.config.manifest_path.as_deref(),
        cli.config.index_dir.as_deref(),
        cli.config.cargo.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("failed to open knowledge index: {e}"))
    .with_context(|| "run 'rust-knowledge index' first (or pass --index-dir / --manifest-path)")
}

fn search(
    cli: &Cli,
    query: &str,
    package: &[String],
    source_kind: &[String],
    item_kind: &[String],
    limit: usize,
    json: bool,
) -> anyhow::Result<()> {
    let retriever = open_retriever(cli)?;
    let query = knowledge_core::SearchQuery {
        text: query.to_string(),
        packages: package.to_vec(),
        source_kinds: parse_source_kinds(source_kind)?,
        item_kinds: item_kind.to_vec(),
        limit,
    };
    let hits = retriever
        .search(&query)
        .map_err(|e| anyhow::anyhow!("search failed: {e}"))?;

    if json {
        for hit in hits {
            println!("{}", serde_json::to_string(&hit)?);
        }
        return Ok(());
    }

    for (n, hit) in hits.iter().enumerate() {
        println!("{}. {}@{}", n + 1, hit.package_name, hit.package_version);
        let context = match (&hit.symbol_path, hit.section_path.is_empty()) {
            (Some(symbol), _) => symbol.clone(),
            (None, false) => hit.section_path.join(" > "),
            (None, true) => hit.title.clone(),
        };
        println!("   {context}");
        println!("   [{}]", hit.source_kind);
        println!("   id: {}", hit.id);
        let text = hit.snippet.replace('\n', " ");
        println!();
        println!("   {text}");
        println!();
    }
    if hits.is_empty() {
        println!("no results");
    }
    Ok(())
}

fn get(cli: &Cli, id: &str, json: bool) -> anyhow::Result<()> {
    let retriever = open_retriever(cli)?;
    let document_id = knowledge_core::DocumentId::from_raw(id)
        .ok_or_else(|| anyhow::anyhow!("{id:?} is not a valid document id"))?;
    let doc = retriever
        .get(&document_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }

    println!("id: {}", doc.id);
    println!("from: {}", doc.provenance());
    println!("context: {}", doc.context());
    if let Some(sig) = &doc.signature {
        println!("signature: {sig}");
    }
    if let Some(path) = &doc.source_path
        && let Some(span) = &doc.source_span
    {
        println!(
            "source: {}:{}-{}",
            path.display(),
            span.start_line,
            span.end_line
        );
    }
    if !doc.related_symbols.is_empty() {
        println!("related: {}", doc.related_symbols.join(", "));
    }
    println!();
    println!("{}", doc.text);
    Ok(())
}

fn symbol_lookup(cli: &Cli, symbol: &str, package: &[String], json: bool) -> anyhow::Result<()> {
    let retriever = open_retriever(cli)?;
    let query = knowledge_core::SymbolQuery {
        symbol: symbol.to_string(),
        packages: package.to_vec(),
        limit: 10,
    };
    let infos = retriever
        .symbol_lookup(&query)
        .map_err(|e| anyhow::anyhow!("symbol lookup failed: {e}"))?;

    if json {
        for info in infos {
            println!("{}", serde_json::to_string(&info)?);
        }
        return Ok(());
    }

    for (n, info) in infos.iter().enumerate() {
        println!(
            "{}. {}@{} [{}] {}",
            n + 1,
            info.package_name,
            info.package_version,
            info.kind,
            info.symbol_path
        );
        if let Some(sig) = &info.signature {
            println!("   {sig}");
        }
        println!("   id: {}", info.id);
        let snippet = info.snippet.replace('\n', " ");
        println!("   {snippet}");
        println!();
    }
    if infos.is_empty() {
        println!("no symbol matched {symbol:?}");
    }
    Ok(())
}

fn eval(cli: &Cli, file: &PathBuf) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read eval file {}", file.display()))?;
    let eval_set = knowledge_index::eval::parse_set(&raw)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", file.display()))?;
    let retriever = open_retriever(cli)?;

    // Eval sets are corpus-specific: refuse to score one against a different
    // workspace instead of producing meaningless numbers.
    if let Some(corpus) = &eval_set.corpus {
        let root = retriever.meta().workspace_root.display().to_string();
        anyhow::ensure!(
            root.ends_with(corpus.trim_end_matches('/')),
            "this eval file targets the {corpus:?} workspace; the current index \
             was built for {root:?}. Point --index-dir/--manifest-path at the \
             matching workspace."
        );
    }

    let outcomes = knowledge_index::eval::run_eval(&retriever, eval_set.cases);
    let summary = knowledge_index::eval::summarize(&outcomes);

    for outcome in &outcomes {
        let status = if outcome.passed() { "PASS" } else { "FAIL" };
        let rank = outcome
            .rank
            .map_or_else(|| "-".to_string(), |r| r.to_string());
        println!(
            "{status} rank {rank:>2} {:>16} {:?}",
            outcome.case.category, outcome.case.text
        );
        if !outcome.passed() {
            println!(
                "     expected one of {:?}, top: {:?}",
                outcome.case.expect_any, outcome.top
            );
        }
    }
    println!(
        "{}/{} passed, MRR {:.3}",
        summary.passed, summary.total, summary.mrr
    );
    if summary.passed != summary.total {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use figue::{DriverError, MockEnv};
    use knowledge_index::config::parse_args_with;

    use super::{ABOUT, Cli, Command, PROGRAM};

    const VERSION: &str = "0.1.0";

    fn parse(argv: &[&str], env: MockEnv) -> figue::DriverOutcome<Cli> {
        parse_args_with(argv, env, PROGRAM, VERSION, ABOUT)
    }

    fn parse_ok(argv: &[&str]) -> Cli {
        parse(argv, MockEnv::new())
            .into_result()
            .expect("argv should parse")
            .get()
    }

    #[test]
    fn parses_both_value_forms_and_repeated_flags() {
        let cli = parse_ok(&[
            "search",
            "tokio spawn",
            "--package",
            "demo_core",
            "--package=base64",
            "--source-kind=rustdoc_item",
            "--source-kind",
            "crate_readme",
            "--item-kind=function",
            "--limit=3",
        ]);
        let Command::Search {
            query,
            package,
            source_kind,
            item_kind,
            limit,
            json,
        } = &cli.command
        else {
            panic!("expected search subcommand, got {:?}", cli.command);
        };
        assert_eq!(query, "tokio spawn");
        assert_eq!(package, &["demo_core".to_string(), "base64".to_string()]);
        assert_eq!(
            source_kind,
            &["rustdoc_item".to_string(), "crate_readme".to_string()]
        );
        assert_eq!(item_kind, &["function".to_string()]);
        assert_eq!(*limit, 3);
        assert!(!json);
    }

    #[test]
    fn defaults_match_the_declared_surface() {
        let cli = parse_ok(&["search", "anything"]);
        let Command::Search {
            query,
            package,
            source_kind,
            item_kind,
            limit,
            json,
        } = &cli.command
        else {
            panic!("expected search subcommand");
        };
        assert_eq!(query, "anything");
        assert!(package.is_empty());
        assert!(source_kind.is_empty());
        assert!(item_kind.is_empty());
        assert_eq!(*limit, 8, "--limit must default to 8");
        assert!(!json);

        let cli = parse_ok(&["dump-docs"]);
        let Command::DumpDocs { rustdoc_scope, .. } = &cli.command else {
            panic!("expected dump-docs subcommand");
        };
        assert_eq!(rustdoc_scope, "workspace");

        // The config root materializes with its declared defaults when no
        // layer provides a value.
        assert_eq!(cli.config.manifest_path, None);
        assert_eq!(cli.config.index_dir, None);
        assert_eq!(cli.config.cargo, None);
        assert_eq!(cli.config.log, "info");
        assert!(!cli.verbose);
    }

    #[test]
    fn global_flags_bind_before_and_after_the_subcommand() {
        let before = parse_ok(&["--manifest-path", "/before/Cargo.toml", "packages"]);
        let after = parse_ok(&["packages", "--manifest-path", "/after/Cargo.toml"]);
        assert_eq!(
            before.config.manifest_path.as_deref(),
            Some(Path::new("/before/Cargo.toml"))
        );
        assert_eq!(
            after.config.manifest_path.as_deref(),
            Some(Path::new("/after/Cargo.toml")),
            "global flags must bind after the subcommand (figue adoption agency)"
        );

        let short = parse_ok(&["packages", "-v"]);
        assert!(short.verbose, "-v must work after the subcommand");
        let long = parse_ok(&["-v", "packages"]);
        assert!(long.verbose, "-v must work before the subcommand");
    }

    #[test]
    fn missing_subcommand_shows_help() {
        // figue renders the top-level help and reports it as a success
        // (exit code 0) when the subcommand is missing.
        match parse(&[], MockEnv::new()).into_result() {
            Err(e @ DriverError::Help { .. }) => {
                assert!(e.is_success());
                let text = format!("{e}");
                assert!(text.contains(PROGRAM), "help text: {text}");
                assert!(text.contains("search"), "help lists subcommands: {text}");
            }
            Err(other) => panic!("expected Help for empty argv, got {other:?}"),
            Ok(_) => panic!("empty argv must not parse"),
        }
    }

    #[test]
    fn missing_positional_shows_help_and_suggestion() {
        // figue's guided failure: when every missing field is a plain CLI
        // argument, it renders the subcommand help plus a corrected-command
        // suggestion and reports it as a success (exit code 0).
        match parse(&["search"], MockEnv::new()).into_result() {
            Err(e @ DriverError::Help { .. }) => {
                assert!(e.is_success());
                assert_eq!(e.exit_code(), 0);
                let text = format!("{e}");
                assert!(text.contains("search"), "help text: {text}");
                assert!(text.contains("<QUERY>"), "help text: {text}");
                assert!(
                    matches!(
                        &e,
                        DriverError::Help {
                            suggestion: Some(_),
                            ..
                        }
                    ),
                    "a missing positional carries a corrected-command suggestion"
                );
            }
            Err(other) => panic!("expected Help for missing QUERY, got {other:?}"),
            Ok(_) => panic!("search without a query must not parse"),
        }
        match parse(&["eval"], MockEnv::new()).into_result() {
            Err(e @ DriverError::Help { .. }) => assert!(e.is_success()),
            Err(other) => panic!("expected Help for missing FILE, got {other:?}"),
            Ok(_) => panic!("eval without a file must not parse"),
        }
    }

    #[test]
    fn unknown_flags_and_subcommands_are_errors() {
        for argv in [
            &["--bogus"][..],
            &["--bogus", "packages"][..],
            &["packages", "--bogus"][..],
            &["frobnicate"][..],
            &["search", "query", "extra"][..],
        ] {
            match parse(argv, MockEnv::new()).into_result() {
                Err(e) => {
                    assert_eq!(e.exit_code(), 1, "figue error exit code for {argv:?}");
                    assert!(!e.is_success());
                }
                Ok(_) => panic!("{argv:?} must not parse"),
            }
        }
    }

    #[test]
    fn help_and_version_short_circuit() {
        for argv in [&["--help"][..], &["-h"][..]] {
            match parse(argv, MockEnv::new()).into_result() {
                Err(e @ DriverError::Help { .. }) => {
                    assert!(e.is_success());
                    let text = format!("{e}");
                    assert!(text.contains("--manifest-path"), "help text: {text}");
                    assert!(text.contains("--index-dir"), "help text: {text}");
                    assert!(text.contains("search"), "help lists subcommands: {text}");
                }
                Err(other) => panic!("expected Help for {argv:?}, got {other:?}"),
                Ok(_) => panic!("{argv:?} must not parse to a value"),
            }
        }
        for argv in [&["--version"][..], &["-V"][..]] {
            match parse(argv, MockEnv::new()).into_result() {
                Err(DriverError::Version { text }) => {
                    assert_eq!(text.trim_end(), "rust-knowledge 0.1.0");
                }
                Err(other) => panic!("expected Version for {argv:?}, got {other:?}"),
                Ok(_) => panic!("{argv:?} must not parse to a value"),
            }
        }
    }

    #[test]
    fn completions_flag_short_circuits() {
        match parse(&["--completions", "zsh"], MockEnv::new()).into_result() {
            Err(e @ DriverError::Completions { .. }) => {
                assert!(e.is_success());
                let script = format!("{e}");
                assert!(script.contains(PROGRAM), "completion script: {script}");
            }
            Err(other) => panic!("expected Completions, got {other:?}"),
            Ok(_) => panic!("--completions must not parse to a value"),
        }
    }

    #[test]
    fn cli_beats_env_end_to_end() {
        // Flag set: it beats the env var.
        let env = MockEnv::from_pairs([("RUST_KNOWLEDGE_INDEX_DIR", "/from-env")]);
        let cli = parse_ok_with_env(&["packages", "--index-dir", "/from-flag"], env);
        assert_eq!(
            cli.config.index_dir.as_deref(),
            Some(Path::new("/from-flag")),
            "CLI args must beat env vars (figue layer precedence)"
        );

        // No flag: the env var fills the gap.
        let env = MockEnv::from_pairs([("RUST_KNOWLEDGE_INDEX_DIR", "/from-env")]);
        let cli = parse_ok_with_env(&["packages"], env);
        assert_eq!(
            cli.config.index_dir.as_deref(),
            Some(Path::new("/from-env"))
        );
    }

    #[test]
    fn cargo_flag_reaches_the_metadata_spawn() {
        // Pins the --cargo plumbing end to end: the flag value must reach
        // the cargo metadata spawn that discovers the workspace when
        // --index-dir is absent (a bad path fails the spawn instead of a
        // silent $PATH fallback).
        let cli = parse_ok(&[
            "--cargo",
            "/definitely-not-a-cargo-binary-0123456789",
            "packages",
        ]);
        let error = match super::open_retriever(&cli) {
            Err(error) => error,
            Ok(_) => panic!("a bad --cargo must fail the metadata spawn"),
        };
        assert!(
            format!("{error:#}").contains("cargo metadata failed"),
            "the bad --cargo must fail the metadata spawn, got: {error:#}"
        );
    }

    #[test]
    fn log_layers_cli_env_and_default() {
        let env = MockEnv::from_pairs([("RUST_LOG", "warn")]);
        let cli = parse_ok_with_env(&["packages"], env);
        assert_eq!(cli.config.log, "warn");

        let env = MockEnv::from_pairs([("RUST_KNOWLEDGE_LOG", "trace")]);
        let cli = parse_ok_with_env(&["packages", "--log", "demo_core=debug"], env);
        assert_eq!(
            cli.config.log, "demo_core=debug",
            "--log must beat the env layer"
        );
    }

    fn parse_ok_with_env(argv: &[&str], env: MockEnv) -> Cli {
        parse(argv, env)
            .into_result()
            .expect("argv should parse")
            .get()
    }
}
