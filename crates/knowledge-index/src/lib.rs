//! Cargo-universe ingestion, corpus normalization and lexical retrieval.
//!
//! Layout (one small module per concern):
//!
//! * [cargo] - the resolved package universe from cargo metadata.
//! * [config] - shared, facet-derived configuration types parsed by figue.
//! * [error] - typed errors with package/command/path context.
//! * [rustdoc] - rustdoc JSON generation (behind a provider interface) and
//!   normalization into [`knowledge_core::KnowledgeDocument`]s.
//! * [markdown] - README/docs/*.md discovery and structural chunking.
//! * [corpus] - assembles the full normalized corpus for a workspace.
//! * [`tantivy_index`] - schema, index build, and the [`KnowledgeRetriever`]
//!   implementation.
//! * [store] - persistent index layout and metadata.
//! * [pipeline] - the end-to-end index command used by CLI and tests.
//! * [eval] - retrieval evaluation over a committed query set.
//! * [telemetry] - the shared layered tracing initializer both binaries
//!   log through (stderr, filter precedence, JSON mode, OTLP seam).

pub mod cargo;
pub mod config;
pub mod corpus;
pub mod error;
pub mod eval;
pub mod markdown;
pub mod pipeline;
pub mod rustdoc;
pub mod store;
pub mod tantivy_index;
pub mod telemetry;

pub use cargo::CargoUniverse;
pub use corpus::{CorpusOptions, CorpusReport, RustdocScope, build_corpus};
pub use error::IndexError;
pub use pipeline::{IndexOptions, IndexOutcome, index_workspace, open_retriever_with};
pub use tantivy_index::TantivyRetriever;
