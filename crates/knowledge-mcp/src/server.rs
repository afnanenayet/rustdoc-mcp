//! The MCP tool surface: `knowledge_search`, `doc_read`, `symbol_lookup`.
//!
//! Tool semantics are designed for agent token economics:
//! 1. search documentation first,
//! 2. inspect compact previews,
//! 3. read only promising documents,
//! 4. open dependency source only when documentation is insufficient.

use knowledge_core::{DocumentId, KnowledgeRetriever as _, SearchQuery, SourceKind, SymbolQuery};
use knowledge_index::TantivyRetriever;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

pub struct KnowledgeServer {
    retriever: TantivyRetriever,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeSearchParams {
    /// Free-form natural-language question or Rust identifiers, e.g.
    /// "how should CPU heavy work interact with async IO" or "`spawn_blocking`".
    #[schemars(description = "Free-form question or Rust identifiers to search documentation for")]
    pub query: String,
    /// Optional package filters: package names or name@version. Empty = all packages.
    #[schemars(description = "Optional: restrict to these packages (name or name@version)")]
    pub packages: Option<Vec<String>>,
    /// Optional source kinds: `rustdoc_item`, `rustdoc_module`, `crate_readme`, `markdown_document`.
    #[schemars(
        description = "Optional: restrict to these source kinds (rustdoc_item, rustdoc_module, crate_readme, markdown_document)"
    )]
    pub source_kinds: Option<Vec<String>>,
    /// Optional item kinds: function, struct, trait, module, ...
    #[schemars(
        description = "Optional: restrict to these item kinds (function, struct, trait, ...)"
    )]
    pub item_kinds: Option<Vec<String>>,
    /// Maximum number of hits (default 8).
    #[schemars(description = "Maximum number of results (default 8)")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocReadParams {
    /// Stable document id as returned by `knowledge_search` or `symbol_lookup`.
    #[schemars(description = "The stable document id from knowledge_search or symbol_lookup")]
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SymbolLookupParams {
    /// Symbol path or last segment, e.g. `tokio::task::spawn_blocking` or `spawn_blocking`.
    #[schemars(
        description = "Symbol path or last segment, e.g. tokio::task::spawn_blocking or spawn_blocking"
    )]
    pub symbol: String,
    /// Optional package filters (name or name@version).
    #[schemars(description = "Optional: restrict to these packages (name or name@version)")]
    pub packages: Option<Vec<String>>,
    /// Maximum number of matches (default 5).
    #[schemars(description = "Maximum number of matches (default 5)")]
    pub limit: Option<usize>,
}

#[tool_router]
impl KnowledgeServer {
    #[must_use]
    pub fn new(retriever: TantivyRetriever) -> Self {
        KnowledgeServer { retriever }
    }

    #[tool(
        description = "Search the documentation and Rust API of every package in this workspace's resolved Cargo dependency graph (natural-language docs, READMEs, rustdoc items). Use this FIRST when investigating Rust dependencies, before searching source files. Returns compact previews (id, package, context, snippet). Then call doc_read with the id of promising hits. Prefer documentation over reading dependency source code."
    )]
    async fn knowledge_search(
        &self,
        Parameters(params): Parameters<KnowledgeSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let source_kinds = match parse_source_kinds(params.source_kinds.as_deref()) {
            Ok(kinds) => kinds,
            Err(message) => return Ok(CallToolResult::error(vec![ContentBlock::text(message)])),
        };
        let query = SearchQuery {
            text: params.query,
            packages: params.packages.unwrap_or_default(),
            source_kinds,
            item_kinds: params.item_kinds.unwrap_or_default(),
            limit: params.limit.unwrap_or(8).clamp(1, 50),
        };
        // Engine failures (queries that sanitize to empty, stale or corrupt
        // indexes) are friendly error results, not protocol errors: an LLM
        // client sending hostile input must get a readable message, exactly
        // the way doc_read reports unknown ids.
        let hits = match self.retriever.search(&query) {
            Ok(hits) => hits,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "search failed: {e}. The index may be stale; run rust-knowledge index."
                ))]));
            }
        };

        let results: Vec<serde_json::Value> = hits
            .iter()
            .map(|hit| {
                let context = match &hit.symbol_path {
                    Some(symbol) => symbol.clone(),
                    None if !hit.section_path.is_empty() => hit.section_path.join(" > "),
                    None => hit.title.clone(),
                };
                serde_json::json!({
                    "id": hit.id.as_str(),
                    "package": hit.package_name,
                    "version": hit.package_version,
                    "kind": hit.source_kind.as_str(),
                    "context": context,
                    "snippet": hit.snippet,
                })
            })
            .collect();
        let content = ContentBlock::json(serde_json::json!({ "results": results }))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    #[tool(
        name = "doc_read",
        description = "Read the full text of one documentation chunk by its stable id (from knowledge_search or symbol_lookup). Includes provenance: package/version, symbol path or heading ancestry, signature, source file and line span. Only read documents that look relevant: search previews are usually enough to decide."
    )]
    async fn doc_read(
        &self,
        Parameters(params): Parameters<DocReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(id) = DocumentId::from_raw(&params.id) else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "{} is not a valid document id; use the id exactly as returned by knowledge_search",
                params.id
            ))]));
        };
        let doc = match self.retriever.get(&id) {
            Ok(doc) => doc,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "no document with id {id}: {e}. The index may be stale; run rust-knowledge index."
                ))]));
            }
        };
        let payload = serde_json::json!({
            "id": doc.id.as_str(),
            "package": doc.package.name,
            "version": doc.package.version,
            "source_kind": doc.source_kind.as_str(),
            "item_kind": doc.item_kind,
            "title": doc.title,
            "symbol_path": doc.symbol_path,
            "section_path": doc.section_path,
            "signature": doc.signature,
            "text": doc.text,
            "source_path": doc.source_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "source_span": doc.source_span,
            "related_symbols": doc.related_symbols,
        });
        let content = ContentBlock::json(payload)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }

    #[tool(
        name = "symbol_lookup",
        description = "Near-exact symbol lookup by path or last segment (e.g. tokio::task::spawn_blocking or spawn_blocking). Returns compact structured API info: kind, signature, doc snippet, source span, related symbols. This is not a search engine: for conceptual or multi-word questions use knowledge_search."
    )]
    async fn symbol_lookup(
        &self,
        Parameters(params): Parameters<SymbolLookupParams>,
    ) -> Result<CallToolResult, McpError> {
        let query = SymbolQuery {
            symbol: params.symbol,
            packages: params.packages.unwrap_or_default(),
            limit: params.limit.unwrap_or(5).clamp(1, 50),
        };
        let infos = match self.retriever.symbol_lookup(&query) {
            Ok(infos) => infos,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "symbol lookup failed: {e}. The index may be stale; run rust-knowledge index."
                ))]));
            }
        };
        let results: Vec<serde_json::Value> = infos
            .iter()
            .map(|info| {
                serde_json::json!({
                    "id": info.id.as_str(),
                    "package": info.package_name,
                    "version": info.package_version,
                    "kind": info.kind,
                    "symbol_path": info.symbol_path,
                    "signature": info.signature,
                    "snippet": info.snippet,
                    "source_path": info.source_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    "source_span": info.source_span,
                    "related_symbols": info.related_symbols,
                })
            })
            .collect();
        let content = ContentBlock::json(serde_json::json!({ "results": results }))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![content]))
    }
}

fn parse_source_kinds(raw: Option<&[String]>) -> Result<Vec<SourceKind>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut kinds = Vec::with_capacity(raw.len());
    for item in raw {
        kinds.push(item.parse::<SourceKind>().map_err(|e| e.clone())?);
    }
    Ok(kinds)
}

#[tool_handler]
#[expect(
    clippy::unused_async_trait_impl,
    reason = "rmcp's tool handler macro requires an async implementation"
)]
impl ServerHandler for KnowledgeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Rust dependency documentation index. Workflow: (1) knowledge_search for APIs or              conceptual questions; (2) doc_read only for promising hits; (3) open dependency              source only if documentation is insufficient. Do not recursively search              ~/.cargo/registry or target directories; this index covers the resolved Cargo graph.",
        )
    }
}
