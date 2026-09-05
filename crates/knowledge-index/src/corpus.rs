//! Assembles the normalized corpus for a workspace: rustdoc items plus
//! Markdown documents, each tagged with exact package provenance.

use cargo_metadata::Package;
use knowledge_core::KnowledgeDocument;
use tracing::{info, info_span, warn};

use crate::cargo::{CargoUniverse, Origin};
use crate::error::IndexError;
use crate::markdown;
use crate::rustdoc::{RustdocProvider, normalize};

/// Which packages get rustdoc JSON generated for them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RustdocScope {
    /// Workspace members and path dependencies (the local crates).
    #[default]
    Workspace,
    /// Everything in the resolved graph; registry failures are skipped.
    All,
    /// No rustdoc generation (Markdown only).
    None,
}

impl RustdocScope {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "workspace" => Some(RustdocScope::Workspace),
            "all" => Some(RustdocScope::All),
            "none" => Some(RustdocScope::None),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CorpusOptions {
    pub rustdoc_scope: RustdocScope,
}

/// Counters reported after a corpus build.
#[derive(Clone, Debug, Default)]
pub struct CorpusReport {
    pub packages: usize,
    pub markdown_files: usize,
    pub rustdoc_packages: usize,
    pub markdown_documents: usize,
    pub rustdoc_documents: usize,
    pub rustdoc_format_version: Option<u32>,
    pub cargo_version: Option<String>,
    pub warnings: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

/// Builds the full normalized corpus for the resolved universe.
///
/// # Errors
///
/// Returns an error when rustdoc normalization fails or a required local
/// rustdoc artifact cannot be generated.
pub fn build_corpus(
    universe: &CargoUniverse,
    provider: &dyn RustdocProvider,
    options: &CorpusOptions,
) -> Result<(Vec<KnowledgeDocument>, CorpusReport), IndexError> {
    let span = info_span!(
        "package_ingestion",
        packages = universe.package_count(),
        documents = tracing::field::Empty
    );
    let _enter = span.enter();
    let start = std::time::Instant::now();

    let mut report = CorpusReport {
        packages: universe.package_count(),
        ..CorpusReport::default()
    };
    let mut documents: Vec<KnowledgeDocument> = Vec::new();

    // --- rustdoc artifacts ---
    let mut rustdoc_packages: Vec<&Package> = Vec::new();
    for pkg in universe.packages() {
        let in_scope = match options.rustdoc_scope {
            RustdocScope::None => false,
            RustdocScope::Workspace => {
                matches!(universe.origin(pkg), Origin::Workspace | Origin::Path)
            }
            RustdocScope::All => true,
        };
        if in_scope {
            rustdoc_packages.push(pkg);
        }
    }

    let generated = provider.generate(universe, &rustdoc_packages)?;

    if options.rustdoc_scope == RustdocScope::Workspace
        && let Some((spec, reason)) = generated.skipped.first()
    {
        // Local packages are required: a failure here is a hard error, never
        // a silent skip.
        return Err(IndexError::RustdocFailed {
            spec: spec.clone(),
            reason: reason.clone(),
        });
    }
    for (spec, reason) in &generated.skipped {
        warn!(package = %spec, reason = %reason, "skipped rustdoc generation");
    }
    report.skipped.clone_from(&generated.skipped);
    for spec in &generated.unsupported {
        info!(package = %spec, "package has no lib target; indexing markdown only");
    }

    for artifact in &generated.artifacts {
        let normalized = normalize(&artifact.package, &artifact.path, universe.workspace_root())?;
        if report.rustdoc_format_version.is_none() {
            report.rustdoc_format_version = Some(normalized.format_version);
        }
        if report.cargo_version.is_none() {
            report.cargo_version.clone_from(&artifact.cargo_version);
        }
        report.warnings.extend(normalized.warnings);
        documents.extend(normalized.documents);
        report.rustdoc_packages += 1;
    }

    // --- markdown documents for every package in the universe ---
    let mut all_pkgs: Vec<&Package> = universe.packages().collect();
    all_pkgs.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    for pkg in &all_pkgs {
        let identity = universe.identity(pkg);
        let readme = pkg.readme.as_ref().map(|p| p.as_std_path());
        let files = markdown::discover(&identity, readme);
        report.markdown_files += files.len();
        for file in files {
            let text = match std::fs::read_to_string(&file.abs_path) {
                Ok(t) => t,
                Err(e) => {
                    warn!(path = %file.abs_path.display(), error = %e, "failed to read markdown file");
                    continue;
                }
            };
            let docs = markdown::chunk_markdown(&identity, &file, &text);
            report.markdown_documents += docs.len();
            documents.extend(docs);
        }
    }

    report.rustdoc_documents = documents
        .iter()
        .filter(|d| d.source_kind.is_rustdoc())
        .count();

    // Deterministic ordering: corpus.jsonl and index contents are stable.
    documents.sort_by(|a, b| {
        a.package
            .display()
            .cmp(&b.package.display())
            .then_with(|| a.context().cmp(&b.context()))
    });

    // Record the final count into the span field declared Empty above so
    // stage telemetry rides along every event inside the span.
    span.record("documents", documents.len());
    info!(
        packages = report.packages,
        rustdoc_packages = report.rustdoc_packages,
        markdown_files = report.markdown_files,
        documents = documents.len(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        "built corpus"
    );

    Ok((documents, report))
}
