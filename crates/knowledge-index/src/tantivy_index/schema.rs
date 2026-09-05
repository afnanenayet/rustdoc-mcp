//! Tantivy schema: field layout, boosts, and document conversion.
//!
//! Boosts (applied at query time via `QueryParser::set_field_boost)`:
//!   `symbol_path` 10  title 5  signature 3  `section_text` 3  `package_name` 2.5
//!   `related_text` 1.5  body 1
//! Plus untokenized exact fields (`symbol_exact` / `symbol_last`) used by term
//! queries with much higher boosts for identifier-shaped queries.
//!
//! Untokenized fields are stored lowercased: tantivy's raw tokenizer does
//! not lowercase, and our exact-match clauses always compare lowercased.

use std::path::PathBuf;

use knowledge_core::{KnowledgeDocument, SourceKind};
use tantivy::TantivyDocument;
use tantivy::schema::{Field, STRING, Schema, TEXT, TextOptions, Value};

use crate::error::IndexError;

/// Bump when the field layout changes; the retriever refuses to read indexes
/// built with a different version.
pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// All index fields, resolved against the schema at open time.
#[derive(Clone, Debug)]
pub struct IndexFields {
    /// Stable document id (hex).
    pub id: Field,
    /// Cargo's opaque package id (provenance only).
    pub package_id: Field,
    /// Tokenized, searchable package name.
    pub package_name: Field,
    /// Raw lowercased package name (filter).
    pub package_name_raw: Field,
    pub package_version: Field,
    /// "name@version" lowercase (filter).
    pub package_key: Field,
    /// "`rustdoc_item`" etc. (filter).
    pub source_kind: Field,
    /// "function" etc. (filter).
    pub item_kind: Field,
    pub title: Field,
    /// Tokenized symbol path, e.g. `demo_core::writer::Writer::write_all`.
    pub symbol_path: Field,
    /// Full path, lowercased, untokenized (exact match).
    pub symbol_exact: Field,
    /// Last path segment, lowercased, untokenized (near-exact match).
    pub symbol_last: Field,
    /// Heading ancestry joined with " > " (searchable context).
    pub section_text: Field,
    /// JSON-encoded Vec<String> heading ancestry (stored).
    pub section_json: Field,
    /// The document text (searchable + stored; the stored copy serves `get()`).
    pub body: Field,
    pub signature: Field,
    /// Related symbols joined (searchable).
    pub related_text: Field,
    /// JSON-encoded related symbols (stored).
    pub related_json: Field,
    pub source_path: Field,
    /// JSON-encoded `SourceSpan` (stored).
    pub source_span_json: Field,
}

/// Tokenized, indexed, stored.
fn text() -> TextOptions {
    TEXT | TextOptions::default().set_stored()
}

/// Tokenized, indexed (not stored).
fn text_unstored() -> TextOptions {
    TEXT
}

/// Tokenized with the English stemmer, indexed, stored. Used for prose
/// (document bodies) so that "errors" matches "error" and "buffered"
/// matches "buffering". Identifier fields stay unstemmed: exactness beats
/// recall for symbols.
fn body() -> TextOptions {
    TextOptions::default().set_stored().set_indexing_options(
        tantivy::schema::TextFieldIndexing::default()
            .set_tokenizer("en_stem")
            .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
    )
}

/// Untokenized, indexed, stored.
fn raw() -> TextOptions {
    STRING | TextOptions::default().set_stored()
}

/// Untokenized, indexed (not stored).
fn raw_unstored() -> TextOptions {
    STRING
}

/// Stored only (not searchable).
fn stored_only() -> TextOptions {
    TextOptions::default().set_stored()
}

#[must_use]
pub fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("id", raw());
    builder.add_text_field("package_id", stored_only());
    builder.add_text_field("package_name", text());
    builder.add_text_field("package_name_raw", raw());
    builder.add_text_field("package_version", stored_only());
    builder.add_text_field("package_key", raw());
    builder.add_text_field("source_kind", raw());
    builder.add_text_field("item_kind", raw());
    builder.add_text_field("title", text());
    builder.add_text_field("symbol_path", text());
    builder.add_text_field("symbol_exact", raw_unstored());
    builder.add_text_field("symbol_last", raw_unstored());
    builder.add_text_field("section_text", text());
    builder.add_text_field("section_json", stored_only());
    builder.add_text_field("body", body());
    builder.add_text_field("signature", text());
    builder.add_text_field("related_text", text_unstored());
    builder.add_text_field("related_json", stored_only());
    builder.add_text_field("source_path", stored_only());
    builder.add_text_field("source_span_json", stored_only());
    builder.build()
}

impl IndexFields {
    /// Resolves all required fields from an existing Tantivy schema.
    ///
    /// # Errors
    ///
    /// Returns an error when a required field is absent.
    pub fn from_schema(schema: &Schema) -> Result<Self, IndexError> {
        fn field(schema: &Schema, name: &str) -> Result<Field, IndexError> {
            schema
                .get_field(name)
                .map_err(|e| IndexError::Engine(format!("index schema field {name:?}: {e}")))
        }
        Ok(IndexFields {
            id: field(schema, "id")?,
            package_id: field(schema, "package_id")?,
            package_name: field(schema, "package_name")?,
            package_name_raw: field(schema, "package_name_raw")?,
            package_version: field(schema, "package_version")?,
            package_key: field(schema, "package_key")?,
            source_kind: field(schema, "source_kind")?,
            item_kind: field(schema, "item_kind")?,
            title: field(schema, "title")?,
            symbol_path: field(schema, "symbol_path")?,
            symbol_exact: field(schema, "symbol_exact")?,
            symbol_last: field(schema, "symbol_last")?,
            section_text: field(schema, "section_text")?,
            section_json: field(schema, "section_json")?,
            body: field(schema, "body")?,
            signature: field(schema, "signature")?,
            related_text: field(schema, "related_text")?,
            related_json: field(schema, "related_json")?,
            source_path: field(schema, "source_path")?,
            source_span_json: field(schema, "source_span_json")?,
        })
    }
}

/// Converts a normalized document into a Tantivy document.
#[must_use]
pub fn to_tantivy_doc(fields: &IndexFields, doc: &KnowledgeDocument) -> TantivyDocument {
    let section_text = doc.section_path.join(" > ");
    let section_json = serde_json::to_string(&doc.section_path).unwrap_or_else(|_| "[]".into());
    let related_text = doc.related_symbols.join(" ");
    let related_json = serde_json::to_string(&doc.related_symbols).unwrap_or_else(|_| "[]".into());
    let span_json = doc
        .source_span
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_default())
        .unwrap_or_default();

    let mut td = TantivyDocument::default();
    td.add_text(fields.id, doc.id.as_str());
    td.add_text(fields.package_id, doc.package.package_id.as_str());
    td.add_text(fields.package_name, doc.package.name.as_str());
    td.add_text(fields.package_name_raw, doc.package.name.to_lowercase());
    td.add_text(fields.package_version, doc.package.version.as_str());
    td.add_text(
        fields.package_key,
        format!("{}@{}", doc.package.name, doc.package.version).to_lowercase(),
    );
    td.add_text(fields.source_kind, doc.source_kind.as_str());
    if let Some(kind) = doc.item_kind.as_deref().filter(|k| !k.is_empty()) {
        // Absence is not a value: an empty item kind must not become a
        // queryable term, or an item_kinds: [""] filter would select every
        // kindless document — a filter bypass.
        td.add_text(fields.item_kind, kind.to_lowercase());
    }
    td.add_text(fields.title, doc.title.as_str());
    td.add_text(
        fields.symbol_path,
        doc.symbol_path.clone().unwrap_or_default(),
    );
    td.add_text(
        fields.symbol_exact,
        doc.symbol_path
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
    );
    td.add_text(
        fields.symbol_last,
        doc.symbol_path
            .as_deref()
            .and_then(|p| p.rsplit("::").next())
            .unwrap_or_default()
            .to_lowercase(),
    );
    td.add_text(fields.section_text, section_text.as_str());
    td.add_text(fields.section_json, section_json.as_str());
    td.add_text(fields.body, doc.text.as_str());
    td.add_text(fields.signature, doc.signature.clone().unwrap_or_default());
    td.add_text(fields.related_text, related_text.as_str());
    td.add_text(fields.related_json, related_json.as_str());
    td.add_text(
        fields.source_path,
        doc.source_path
            .as_ref()
            .map(|p| p.to_string_lossy())
            .unwrap_or_default(),
    );
    td.add_text(fields.source_span_json, span_json.as_str());
    td
}

/// Rebuilds a normalized document from stored fields.
pub fn from_tantivy_doc(fields: &IndexFields, doc: &TantivyDocument) -> Option<KnowledgeDocument> {
    fn str_of(doc: &TantivyDocument, field: Field) -> Option<String> {
        doc.get_first(field)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
    let package_id = str_of(doc, fields.package_id)?;
    let name = str_of(doc, fields.package_name)?;
    let version = str_of(doc, fields.package_version)?;
    let source_kind = str_of(doc, fields.source_kind).and_then(|s| s.parse::<SourceKind>().ok())?;

    let section_path: Vec<String> = str_of(doc, fields.section_json)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let related_symbols: Vec<String> = str_of(doc, fields.related_json)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let source_span =
        str_of(doc, fields.source_span_json).and_then(|s| serde_json::from_str(&s).ok());

    Some(KnowledgeDocument {
        id: knowledge_core::DocumentId::from_raw(str_of(doc, fields.id)?)?,
        package: knowledge_core::PackageIdentity {
            package_id,
            name,
            version,
            source: None,
            manifest_path: PathBuf::new(),
        },
        source_kind,
        title: str_of(doc, fields.title).unwrap_or_default(),
        symbol_path: str_of(doc, fields.symbol_path).filter(|s| !s.is_empty()),
        item_kind: str_of(doc, fields.item_kind).filter(|s| !s.is_empty()),
        section_path,
        text: str_of(doc, fields.body).unwrap_or_default(),
        source_path: str_of(doc, fields.source_path)
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty()),
        source_span,
        related_symbols,
        signature: str_of(doc, fields.signature).filter(|s| !s.is_empty()),
    })
}
