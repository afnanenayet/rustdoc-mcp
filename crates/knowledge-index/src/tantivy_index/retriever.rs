//! The Tantivy-backed [`KnowledgeRetriever`].
//!
//! Query construction, in one place:
//! * free text: `QueryParser` over boosted fields (`symbol_path` 10, title 5,
//!   signature 3, `section_text` 3, `package_name` 2.5, `related_text` 1.5, body 1);
//! * identifier-shaped tokens (CamelCase, `snake_case`, `::-paths`) additionally
//!   produce raw term queries on `symbol_exact` (boost 30) and `symbol_last`
//!   (boost 12/8), so exact symbol matches dominate identifier queries;
//! * filters (packages, source kinds, item kinds) are `BooleanQuery` MUST
//!   clauses; no Tantivy query syntax is ever exposed to callers.

use std::path::{Path, PathBuf};

use knowledge_core::{
    DocumentId, KnowledgeDocument, KnowledgeError, KnowledgeRetriever, Result, SearchHit,
    SearchQuery, SourceKind, SymbolInfo, SymbolQuery,
};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, TantivyDocument, Term};
use tracing::{info, info_span};

use crate::error::IndexError;
use crate::store::IndexMeta;
use crate::tantivy_index::schema::{INDEX_SCHEMA_VERSION, IndexFields, from_tantivy_doc};

/// How long a snippet may be.
const SNIPPET_CHARS: usize = 300;

pub struct TantivyRetriever {
    index: Index,
    reader: tantivy::IndexReader,
    fields: IndexFields,
    meta: IndexMeta,
    index_dir: PathBuf,
}

impl TantivyRetriever {
    /// Opens an existing index (built by `build_index` / `index_workspace`).
    ///
    /// # Errors
    ///
    /// Returns an error when metadata or the Tantivy index is missing,
    /// malformed, or cannot be opened.
    pub fn open(index_dir: &Path) -> Result<Self> {
        let meta_path = IndexMeta::meta_path(index_dir);
        let meta = IndexMeta::load(&meta_path).map_err(|e| match e {
            IndexError::Knowledge(k) => k,
            other => KnowledgeError::Engine(other.to_string()),
        })?;
        if meta.schema_version() != INDEX_SCHEMA_VERSION {
            return Err(KnowledgeError::SchemaVersion {
                path: index_dir.to_path_buf(),
                index: meta.schema_version(),
                supported: INDEX_SCHEMA_VERSION,
            });
        }
        let tantivy_dir = IndexMeta::tantivy_dir(index_dir);
        let index = Index::open_in_dir(&tantivy_dir).map_err(|e| {
            KnowledgeError::Engine(format!("open index at {}: {e}", tantivy_dir.display()))
        })?;
        let fields = IndexFields::from_schema(&index.schema())
            .map_err(|e| KnowledgeError::Engine(e.to_string()))?;
        let reader = index
            .reader()
            .map_err(|e| KnowledgeError::Engine(format!("open index reader: {e}")))?;
        reader
            .reload()
            .map_err(|e| KnowledgeError::Engine(format!("reload index: {e}")))?;

        Ok(TantivyRetriever {
            index,
            reader,
            fields,
            meta,
            index_dir: index_dir.to_path_buf(),
        })
    }

    #[must_use]
    pub fn meta(&self) -> &IndexMeta {
        &self.meta
    }

    /// The normalized corpus stored beside the index (debug tooling).
    ///
    /// # Errors
    ///
    /// Returns an error when the stored corpus cannot be read or decoded.
    pub fn read_corpus(&self) -> Result<Vec<KnowledgeDocument>> {
        crate::store::read_corpus(&self.index_dir)
            .map_err(|e| KnowledgeError::Engine(e.to_string()))
    }

    fn searcher(&self) -> tantivy::Searcher {
        self.reader.searcher()
    }

    /// Clamps a caller-supplied limit for [TopDocs::with_limit].
    ///
    /// * `0` is treated as `1` (a positive limit is required);
    /// * oversized values are capped at the number of live documents:
    ///   tantivy's top collector overflows on `usize::MAX`-scale limits, and
    ///   a limit above the corpus size returns the same hits anyway.
    fn clamped_limit(&self, requested: usize) -> usize {
        let num_docs = self.searcher().num_docs().max(1);
        requested.max(1).min(num_docs as usize)
    }

    fn query_parser(&self, fields: &[Field]) -> QueryParser {
        let mut parser = QueryParser::for_index(&self.index, fields.to_vec());
        for (field, boost) in [
            (self.fields.symbol_path, 10.0),
            (self.fields.title, 5.0),
            (self.fields.signature, 3.0),
            (self.fields.section_text, 3.0),
            (self.fields.package_name, 2.5),
            (self.fields.related_text, 1.5),
            (self.fields.body, 1.0),
        ] {
            parser.set_field_boost(field, boost);
        }
        parser
    }

    /// Parses text strictly; if the grammar rejects it, re-parses the
    /// syntax-free words. The lenient parser is deliberately not used:
    /// it happily returns degenerate leaves (unclosed ranges, half-built
    /// sets) alongside its error list, and executing those can trip
    /// scorer invariants inside tantivy. If even the plain words fail to
    /// parse (lone boolean keywords), the empty BooleanQuery matches
    /// nothing — a structured dead end, never a panic.
    fn parsed_query(&self, fields: &[Field], text: &str) -> Box<dyn tantivy::query::Query> {
        let parser = self.query_parser(fields);
        match parser.parse_query(&grammar_escaped(text)) {
            Ok(parsed) => parsed,
            Err(_) => {
                let words = syntax_free_text(text);
                match parser.parse_query(&words) {
                    Ok(parsed) => parsed,
                    Err(_) => Box::new(BooleanQuery::from(Vec::<
                        (Occur, Box<dyn tantivy::query::Query>),
                    >::new())),
                }
            }
        }
    }

    /// Identifier-shaped tokens become high-boost exact term queries.
    fn identifier_clauses(&self, text: &str) -> Vec<(Occur, Box<dyn tantivy::query::Query>)> {
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
        for raw in text.split_whitespace() {
            let token: String = raw
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            if token.len() < 3 {
                continue;
            }
            let lower = token.to_lowercase();
            if lower.contains("::") {
                clauses.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.symbol_exact, &lower),
                            IndexRecordOption::Basic,
                        )),
                        30.0,
                    )),
                ));
                let last = lower.rsplit("::").next().unwrap_or_default().to_string();
                if last.len() >= 2 {
                    clauses.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.symbol_last, &last),
                                IndexRecordOption::Basic,
                            )),
                            12.0,
                        )),
                    ));
                }
            } else if raw.contains('_') || raw.chars().any(char::is_uppercase) || raw.contains("::")
            {
                // snake_case / CamelCase identifiers: strong last-segment boost.
                clauses.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.symbol_last, &lower),
                            IndexRecordOption::Basic,
                        )),
                        12.0,
                    )),
                ));
            } else if token.chars().all(char::is_alphanumeric) && lower.len() >= 4 {
                // Plain words get a gentle last-segment boost so symbol
                // matches surface early, without distorting prose queries.
                clauses.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.symbol_last, &lower),
                            IndexRecordOption::Basic,
                        )),
                        8.0,
                    )),
                ));
            }
        }
        clauses
    }

    fn filter_clause(&self, query: &SearchQuery) -> Option<Box<dyn tantivy::query::Query>> {
        let mut must: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

        if !query.packages.is_empty() {
            let mut package_clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for package in &query.packages {
                let spec = package.trim().to_lowercase();
                if spec.is_empty() {
                    // An empty spec is garbage, not a wildcard. Skipping it
                    // must never lift the filter: if every spec is empty the
                    // clause list stays empty and the BooleanQuery below
                    // matches nothing (tantivy maps an empty BooleanQuery to
                    // an EmptyScorer).
                    continue;
                }
                if let Some((name, version)) = spec.split_once('@') {
                    package_clauses.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(
                                self.fields.package_key,
                                &format!("{name}@{version}"),
                            ),
                            IndexRecordOption::Basic,
                        )),
                    ));
                } else {
                    package_clauses.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.package_name_raw, &spec),
                            IndexRecordOption::Basic,
                        )),
                    ));
                }
            }
            must.push((Occur::Must, Box::new(BooleanQuery::from(package_clauses))));
        }

        if !query.source_kinds.is_empty() {
            let mut kind_clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for kind in &query.source_kinds {
                kind_clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.source_kind, kind.as_str()),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            must.push((Occur::Must, Box::new(BooleanQuery::from(kind_clauses))));
        }

        if !query.item_kinds.is_empty() {
            let mut kind_clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for kind in &query.item_kinds {
                // Same normalization as package specs: trim + lowercase, and
                // an empty spec is garbage that must select nothing rather
                // than widen the filter to every kindless document.
                let spec = kind.trim().to_lowercase();
                if spec.is_empty() {
                    continue;
                }
                kind_clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.item_kind, &spec),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            must.push((Occur::Must, Box::new(BooleanQuery::from(kind_clauses))));
        }

        if must.is_empty() {
            None
        } else if must.len() == 1 {
            must.pop().map(|(_, q)| q)
        } else {
            Some(Box::new(BooleanQuery::from(must)))
        }
    }

    /// Builds the tantivy query. The text argument is the caller's query
    /// after [searchable_text] sanitization (punctuation-only tokens are
    /// dropped, so hostile syntax never reaches the grammar).
    fn build_search_query(
        &self,
        text: &str,
        query: &SearchQuery,
    ) -> Option<Box<dyn tantivy::query::Query>> {
        if text.is_empty() {
            return self.filter_clause(query);
        }
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
        clauses.push((Occur::Should, self.parsed_query(&[
            self.fields.symbol_path,
            self.fields.title,
            self.fields.section_text,
            self.fields.package_name,
            self.fields.signature,
            self.fields.related_text,
            self.fields.body,
        ], text)));
        clauses.extend(self.identifier_clauses(text));
        if let Some(filter) = self.filter_clause(query) {
            clauses.push((Occur::Must, filter));
        }
        Some(Box::new(BooleanQuery::from(clauses)))
    }

    fn snippet_for(&self, query: &str, doc: &TantivyDocument) -> String {
        let body_text = doc
            .get_first(self.fields.body)
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if body_text.trim().is_empty() {
            return self.signature_or_title(doc);
        }
        let body_query = self.parsed_query(&[self.fields.body], query);
        let searcher = self.searcher();
        let snippet = match SnippetGenerator::create(&searcher, &body_query, self.fields.body) {
            Ok(mut generator) => {
                generator.set_max_num_chars(SNIPPET_CHARS);
                let snippet = generator.snippet_from_doc(doc);
                snippet.fragment().trim().to_string()
            }
            Err(_) => String::new(),
        };
        if snippet.is_empty() {
            // No body term matched; take the opening of the document.
            truncate_at_word(body_text, SNIPPET_CHARS)
        } else {
            snippet
        }
    }

    fn signature_or_title(&self, doc: &TantivyDocument) -> String {
        if let Some(sig) = doc
            .get_first(self.fields.signature)
            .and_then(|v| v.as_str())
            && !sig.is_empty()
        {
            return sig.to_string();
        }
        let title = doc
            .get_first(self.fields.title)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let body = doc
            .get_first(self.fields.body)
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if body.is_empty() {
            title
        } else {
            truncate_at_word(body, SNIPPET_CHARS)
        }
    }

    fn hit_from_doc(&self, score: f32, doc: &TantivyDocument) -> Option<SearchHit> {
        let id = DocumentId::from_raw(
            doc.get_first(self.fields.id)
                .and_then(|v| v.as_str())
                .map(str::to_string)?,
        )?;
        let section_path: Vec<String> = doc
            .get_first(self.fields.section_json)
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        Some(SearchHit {
            id,
            package_name: doc
                .get_first(self.fields.package_name)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            package_version: doc
                .get_first(self.fields.package_version)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            source_kind: doc
                .get_first(self.fields.source_kind)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<SourceKind>().ok())
                .unwrap_or(SourceKind::MarkdownDocument),
            title: doc
                .get_first(self.fields.title)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            symbol_path: doc
                .get_first(self.fields.symbol_path)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            section_path,
            snippet: String::new(),
            score,
        })
    }
}

impl KnowledgeRetriever for TantivyRetriever {
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let span = info_span!("search", query = %query.text);
        let _enter = span.enter();
        let start = std::time::Instant::now();

        let limit = self.clamped_limit(query.limit);
        let text = searchable_text(&query.text);
        let tantivy_query = self
            .build_search_query(&text, query)
            .ok_or_else(|| KnowledgeError::Engine("empty query".into()))?;
        let searcher = self.searcher();
        let top = searcher
            .search(&tantivy_query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(KnowledgeError::engine)?;

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr).map_err(KnowledgeError::engine)?;
            if let Some(mut hit) = self.hit_from_doc(score, &doc) {
                hit.snippet = self.snippet_for(&text, &doc);
                hits.push(hit);
            }
        }
        info!(
            hits = hits.len(),
            elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            "search done"
        );
        Ok(hits)
    }

    fn get(&self, id: &DocumentId) -> Result<KnowledgeDocument> {
        let searcher = self.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.id, id.as_str()),
            IndexRecordOption::Basic,
        );
        let top = searcher
            .search(&query, &TopDocs::with_limit(1).order_by_score())
            .map_err(KnowledgeError::engine)?;
        let (_, addr) = top
            .first()
            .ok_or_else(|| KnowledgeError::DocumentNotFound(id.clone()))?;
        let doc: TantivyDocument = searcher.doc(*addr).map_err(KnowledgeError::engine)?;
        from_tantivy_doc(&self.fields, &doc)
            .ok_or_else(|| KnowledgeError::DocumentNotFound(id.clone()))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "symbol lookup keeps its scoring and result projection together"
    )]
    fn symbol_lookup(&self, query: &SymbolQuery) -> Result<Vec<SymbolInfo>> {
        let span = info_span!("symbol_lookup", symbol = %query.symbol);
        let _enter = span.enter();

        let symbol = query.symbol.trim().to_lowercase();
        if symbol.is_empty() {
            return Ok(Vec::new());
        }

        let exact = Box::new(BoostQuery::new(
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.symbol_exact, &symbol),
                IndexRecordOption::Basic,
            )),
            100.0,
        ));
        let last_segment = symbol.rsplit("::").next().unwrap_or_default().to_string();
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
            (Occur::Should, exact),
            (
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.symbol_last, &symbol),
                        IndexRecordOption::Basic,
                    )),
                    50.0,
                )),
            ),
        ];
        if last_segment != symbol {
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.symbol_last, &last_segment),
                        IndexRecordOption::Basic,
                    )),
                    40.0,
                )),
            ));
        }
        // Qualified queries: AND over every query token on the tokenized
        // symbol path; this ranks the fully matching path above bare
        // last-segment ties.
        let path_tokens: Vec<String> = symbol
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        if symbol.contains("::") && path_tokens.len() > 1 {
            let seg_clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = path_tokens
                .iter()
                .map(|s| {
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.symbol_path, s),
                            IndexRecordOption::Basic,
                        )) as Box<dyn tantivy::query::Query>,
                    )
                })
                .collect();
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(BooleanQuery::from(seg_clauses)),
                    30.0,
                )),
            ));
        }

        if !query.packages.is_empty() {
            let filter = self.filter_clause(&SearchQuery {
                text: String::new(),
                packages: query.packages.clone(),
                source_kinds: Vec::new(),
                item_kinds: Vec::new(),
                limit: query.limit,
            });
            if let Some(filter) = filter {
                clauses.push((Occur::Must, filter));
            }
        }

        let boolean = BooleanQuery::from(clauses);
        let searcher = self.searcher();
        let top = searcher
            .search(
                &boolean,
                &TopDocs::with_limit(self.clamped_limit(query.limit)).order_by_score(),
            )
            .map_err(KnowledgeError::engine)?;

        let mut out = Vec::with_capacity(top.len());
        for (_score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr).map_err(KnowledgeError::engine)?;
            let Some(symbol_path) = doc
                .get_first(self.fields.symbol_path)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let body = doc
                .get_first(self.fields.body)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let related: Vec<String> = doc
                .get_first(self.fields.related_json)
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let span_value: Option<knowledge_core::SourceSpan> = doc
                .get_first(self.fields.source_span_json)
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok());
            out.push(SymbolInfo {
                id: DocumentId::from_raw(
                    doc.get_first(self.fields.id)
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_default(),
                )
                .unwrap_or_else(|| DocumentId::from_identity(&[])),
                package_name: doc
                    .get_first(self.fields.package_name)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                package_version: doc
                    .get_first(self.fields.package_version)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                kind: doc
                    .get_first(self.fields.item_kind)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map_or_else(|| "module".to_string(), str::to_string),
                symbol_path,
                signature: doc
                    .get_first(self.fields.signature)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                snippet: if body.trim().is_empty() {
                    doc.get_first(self.fields.signature)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                } else {
                    truncate_at_word(body, 240)
                },
                source_path: doc
                    .get_first(self.fields.source_path)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from),
                source_span: span_value,
                related_symbols: related,
            });
        }
        Ok(out)
    }
}

/// Query text with punctuation-only tokens removed.
///
/// A token containing no alphanumeric characters (a bare asterisk, a lone
/// quote, a run of colons) can never match an indexed term, and tantivy's
/// query grammar panics on an unprefixed asterisk: it parses as an "exists"
/// query without a field, and UserInputLeaf::set_field asserts the field is
/// present. LLM clients send such tokens freely, so they are dropped before
/// the text reaches the parser; for every real query this changes nothing.
fn searchable_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for token in raw.split_whitespace() {
        if !token.chars().any(char::is_alphanumeric) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }
    out
}

/// Query text reduced to plain words, with every bit of tantivy query
/// grammar syntax removed (field prefixes, occur/boost markers, ranges,
/// sets, phrases, regexes). Used to re-parse text the grammar rejected:
/// executing a broken AST (an unclosed range, a half-built set) can trip
/// scorer invariants deep inside tantivy, so it is never attempted.
fn syntax_free_text(raw: &str) -> String {
    raw.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Escapes the one character whose unescaped presence can make
/// tantivy's grammar panic while building the AST: a bare asterisk is
/// parsed as an "exists" query, which asserts when no field precedes it
/// (and the grammar can see one mid-token, e.g. "*[A"). Escaped, the
/// asterisk becomes a literal term character that tokenization splits
/// on exactly as it does today, so wildcard syntax — deliberately not
/// part of the retriever contract — is the only behavior lost.
/// Backslashes are escaped first so user input cannot pre-escape ours.
fn grammar_escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('*', "\\*")
}

/// Truncates text to at most max chars, breaking on a word boundary.
pub(crate) fn truncate_at_word(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max {
        return trimmed.to_string();
    }
    let mut end = max;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let head = trimmed.get(..end).unwrap_or_default();
    match head.rfind(char::is_whitespace) {
        Some(space) if space > max / 2 => {
            format!("{}...", head.get(..space).unwrap_or_default().trim_end())
        }
        _ => format!("{head}..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_word_boundary() {
        let text = "Runs the provided closure on a thread where blocking operations are acceptable";
        let cut = truncate_at_word(text, 30);
        assert!(cut.len() <= 33, "cut: {cut:?}");
        assert!(cut.ends_with("..."));
        assert!(!cut.ends_with(" ..."));
    }

    #[test]
    fn short_text_passes_through() {
        assert_eq!(truncate_at_word("short text", 100), "short text");
    }

    #[test]
    fn grammar_escaped_neutralizes_exists_markers() {
        // A bare asterisk anywhere would make the grammar build an
        // "exists" query, which panics without a field; escaped it is a
        // literal term character.
        assert_eq!(grammar_escaped("*[A"), "\\*[A");
        assert_eq!(grammar_escaped("a*b"), "a\\*b");
        assert_eq!(grammar_escaped("*"), "\\*");
        // User backslashes are escaped first, so they cannot un-escape ours:
        // a\*b becomes a literal backslash plus a literal asterisk.
        assert_eq!(grammar_escaped("a\\*b"), "a\\\\\\*b");
        assert_eq!(grammar_escaped("plain text"), "plain text");
    }

    #[test]
    fn syntax_free_text_strips_all_grammar() {
        // Range/set/occur/field syntax is reduced to plain words, which
        // can never build anything but term queries.
        assert_eq!(syntax_free_text("-G{a["), "G a");
        assert_eq!(syntax_free_text("title:[a TO z]"), "title a TO z");
        assert_eq!(syntax_free_text("+must -mustnot ^boost"), "must mustnot boost");
        assert_eq!(syntax_free_text("Writer::flush"), "Writer flush");
        assert_eq!(syntax_free_text(r#""a phrase"~2"#), "a phrase 2");
        assert_eq!(syntax_free_text("*"), "");
    }

    #[test]
    fn searchable_text_drops_punctuation_only_tokens() {
        // The shapes that panic tantivy's grammar (an "exists" query
        // without a field) must be gone before parsing.
        assert_eq!(searchable_text("* * ** *** *?*"), "");
        assert_eq!(searchable_text("*"), "");
        // Field-scoped syntax and real tokens survive untouched.
        assert_eq!(searchable_text("a:* b"), "a:* b");
        assert_eq!(searchable_text("write_all *"), "write_all");
        assert_eq!(
            searchable_text("  how * should ::  this  "),
            "how should this"
        );
        // Quoted phrases keep their quotes: the token has word characters.
        assert_eq!(searchable_text("\"writer\" *"), "\"writer\"");
        // Unicode word text is kept; unicode punctuation is not.
        assert_eq!(searchable_text("\u{4e2d}\u{6587} *"), "\u{4e2d}\u{6587}");
    }

    #[test]
    fn respects_char_boundaries() {
        // Multi-byte characters must not split mid-character.
        let text = "ä".repeat(50);
        let cut = truncate_at_word(&text, 20);
        assert!(cut.chars().all(|c| c == 'ä' || c == '.'));
    }
}
