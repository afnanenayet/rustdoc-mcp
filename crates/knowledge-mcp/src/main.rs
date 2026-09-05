//! Binary entry point: serves the knowledge MCP over stdio.
//!
//! Logs go to stderr (stdout is the MCP protocol channel).
//!
//! Typical client configuration:
//! {"mcpServers": {"rust-knowledge": {"command": "knowledge-mcp",
//!   `args`: [`--manifest-path`, `/path/to/workspace/Cargo.toml`]}}}
//!
//! Arguments are parsed by figue over a facet shape (issue #2): the
//! flattened config root layers CLI flags over the `RUST_KNOWLEDGE`_* env
//! vars over defaults with figue's own precedence, and
//! `figue::FigueBuiltins` contributes --help/--version and friends.
//! `--cargo` is honored on the cargo metadata run that locates the
//! workspace when `--index-dir` is absent (the documented deployment
//! passes only `--manifest-path`, so that run is the common case).

use std::process::ExitCode;

use facet::Facet;
use figue::{self as args, FigueBuiltins};
use knowledge_index::config::{WorkspaceConfig, parse_std_args};
use knowledge_index::telemetry::TelemetryOptions;
use knowledge_mcp::KnowledgeServer;
use rmcp::service::serve_server;
use rmcp::transport::stdio;

/// User-facing description of the `knowledge-mcp` binary.
///
/// Single source for every surface that describes the binary: the figue
/// help configuration passed by its `main` and any generated reference.
const DESCRIPTION: &str = "MCP server exposing the rust-knowledge retrieval engine";

/// Full argument surface of the `knowledge-mcp` binary.
#[derive(Facet, Debug)]
struct McpArgs {
    /// Workspace knobs, layered by figue (CLI > env > file > defaults).
    #[facet(args::config, args::env_prefix = "RUST_KNOWLEDGE", flatten)]
    config: WorkspaceConfig,

    /// Standard figue builtins: --help, --html-help, --version,
    /// --completions, --export-jsonschemas.
    #[facet(flatten)]
    builtins: FigueBuiltins,
}

fn main() -> ExitCode {
    let cli =
        parse_std_args::<McpArgs>("knowledge-mcp", env!("CARGO_PKG_VERSION"), DESCRIPTION).unwrap();
    // MCP speaks JSON-RPC on stdout; the shared layered init writes every
    // log to stderr, so stdout stays structurally pristine. The filter comes
    // from RUST_KNOWLEDGE_LOG -> RUST_LOG -> the built-in default.
    knowledge_index::telemetry::init(&TelemetryOptions::default());

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = runtime.block_on(run(&cli.config)) {
        eprintln!("error: {e:#}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Opens the index for the parsed config: an explicit `--index-dir`
/// wins; otherwise a `cargo metadata` run discovers the workspace for
/// the default location — with the config's `--cargo` binary, so the
/// flag is honored on this spawn too (the documented deployment passes
/// only `--manifest-path`, so the discovery run is the common case).
fn open_retriever(config: &WorkspaceConfig) -> anyhow::Result<knowledge_index::TantivyRetriever> {
    knowledge_index::open_retriever_with(
        config.manifest_path.as_deref(),
        config.index_dir.as_deref(),
        config.cargo.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!(
        "failed to open the knowledge index: {e}. Build it first with 'rust-knowledge index' (or pass --index-dir)."
    ))
}

async fn run(config: &WorkspaceConfig) -> anyhow::Result<()> {
    let retriever = open_retriever(config)?;
    tracing::info!(
        index = retriever.meta().workspace_root.display().to_string(),
        documents = retriever.meta().document_count,
        "serving knowledge MCP on stdio"
    );

    let server = KnowledgeServer::new(retriever);
    let service = serve_server(server, stdio())
        .await
        .map_err(|e| anyhow::anyhow!("failed to start MCP server: {e:?}"))?;
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("server task failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use figue::MockEnv;
    use knowledge_index::config::parse_args_with;

    use super::{DESCRIPTION, McpArgs};

    fn parse(argv: &[&str], env: MockEnv) -> McpArgs {
        parse_args_with::<McpArgs>(
            argv,
            env,
            "knowledge-mcp",
            env!("CARGO_PKG_VERSION"),
            DESCRIPTION,
        )
        .into_result()
        .expect("argv should parse")
        .get()
    }

    /// Pins the --cargo plumbing end to end: the flag value must reach the
    /// cargo metadata spawn that discovers the workspace when --index-dir
    /// is absent (a bad path fails the spawn instead of a silent $PATH
    /// fallback). Mirrors the reviewer's probe against this binary.
    #[test]
    fn cargo_flag_reaches_the_metadata_spawn() {
        let args = parse(
            &["--cargo", "/definitely-not-a-cargo-binary-0123456789"],
            MockEnv::new(),
        );
        let error = match super::open_retriever(&args.config) {
            Err(error) => error,
            Ok(_) => panic!("a bad --cargo must fail the metadata spawn"),
        };
        assert!(
            format!("{error:#}").contains("cargo metadata failed"),
            "the bad --cargo must fail the metadata spawn, got: {error:#}"
        );
    }

    /// Pins the env wiring of the REAL [McpArgs] shape, not the TestArgs
    /// lookalike in knowledge-index's config tests: the flattened config
    /// root must keep reading $RUST_KNOWLEDGE_* — if its env_prefix or
    /// flatten attribute drifts (a typo, an accidental rename), every
    /// lookalike precedence test stays green while the deployed server
    /// silently stops honoring every env var.
    #[test]
    fn env_layer_addresses_the_real_shape() {
        let env = MockEnv::from_pairs([
            ("RUST_KNOWLEDGE_INDEX_DIR", "/idx-from-env"),
            ("RUST_KNOWLEDGE_CARGO", "/cargo-from-env"),
            ("RUST_KNOWLEDGE_LOG", "warn"),
        ]);
        let args = parse(&[], env);
        assert_eq!(
            args.config.index_dir.as_deref(),
            Some(Path::new("/idx-from-env")),
            "the real shape must read $RUST_KNOWLEDGE_INDEX_DIR"
        );
        assert_eq!(
            args.config.cargo.as_deref(),
            Some(Path::new("/cargo-from-env")),
            "the real shape must read $RUST_KNOWLEDGE_CARGO"
        );
        assert_eq!(args.config.log, "warn");

        // And a flag still beats the env layer on the real shape.
        let env = MockEnv::from_pairs([("RUST_KNOWLEDGE_INDEX_DIR", "/idx-from-env")]);
        let args = parse(&["--index-dir", "/idx-from-flag"], env);
        assert_eq!(
            args.config.index_dir.as_deref(),
            Some(Path::new("/idx-from-flag")),
            "a CLI flag must beat the env layer on the real shape"
        );
    }
}
