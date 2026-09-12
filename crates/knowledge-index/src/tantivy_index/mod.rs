//! Tantivy-backed index build and retrieval.
//!
//! Indexing is the expensive preprocessing step; search re-opens the
//! persistent index and never touches cargo, rustdoc, or the corpus sources.

pub mod retriever;
pub mod schema;

use std::path::Path;

use tantivy::Index;

use crate::error::IndexError;
use crate::store::IndexMeta;
pub use retriever::TantivyRetriever;

/// Builds (or rebuilds) the index at `index_dir` from a corpus.
///
/// # Errors
///
/// Returns an error when the index directory cannot be prepared or Tantivy
/// cannot write the index.
pub fn build_index(
    index_dir: &Path,
    documents: &[knowledge_core::KnowledgeDocument],
    meta: &IndexMeta,
) -> Result<(), IndexError> {
    let span = tracing::info_span!("index_build", documents = documents.len());
    let _enter = span.enter();
    let start = std::time::Instant::now();

    std::fs::create_dir_all(index_dir).map_err(|e| IndexError::io(index_dir, e))?;
    let tantivy_dir = IndexMeta::tantivy_dir(index_dir);
    // A stale index would poison the new one; rebuilds start clean.
    // (Note: tantivy's create_in_dir requires the directory to exist.)
    if tantivy_dir.exists() {
        std::fs::remove_dir_all(&tantivy_dir).map_err(|e| IndexError::io(&tantivy_dir, e))?;
    }
    std::fs::create_dir_all(&tantivy_dir).map_err(|e| IndexError::io(&tantivy_dir, e))?;

    let schema = schema::build_schema();
    let index = Index::create_in_dir(&tantivy_dir, schema).map_err(IndexError::from)?;
    let fields = schema::IndexFields::from_schema(&index.schema())?;
    let mut writer = index.writer(50 * 1024 * 1024).map_err(IndexError::from)?;
    for doc in documents {
        let tantivy_doc = schema::to_tantivy_doc(&fields, doc);
        writer.add_document(tantivy_doc).map_err(IndexError::from)?;
    }
    writer.commit().map_err(IndexError::from)?;
    writer.wait_merging_threads().map_err(IndexError::from)?;

    crate::store::write_corpus(index_dir, documents)?;
    meta.save(&IndexMeta::meta_path(index_dir))?;

    tracing::info!(
        index_dir = %index_dir.display(),
        documents = documents.len(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "index built"
    );
    Ok(())
}
