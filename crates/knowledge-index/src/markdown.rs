//! README and Markdown discovery plus structural chunking.
//!
//! Markdown is never chunked by character count: chunks follow the heading
//! structure, retaining the full heading ancestry on every chunk. Oversized
//! sections are split at block boundaries deterministically. Where Cargo
//! metadata names a README explicitly, that wins over filename scanning.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use knowledge_core::{DocumentId, KnowledgeDocument, PackageIdentity, SourceKind, SourceSpan};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use tracing::{debug, info, info_span};

/// A discovered Markdown input file.
#[derive(Clone, Debug)]
pub struct MarkdownFile {
    /// Path relative to the package root, forward slashes (e.g. README.md,
    /// docs/guide.md). Part of the document identity.
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub kind: SourceKind,
}

/// Conservative rules: never descend into these directories.
const SKIP_DIRS: [&str; 5] = ["target", ".git", "node_modules", "vendor", "tests"];
/// Files larger than this are skipped (never index generated megabytes).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Discovers the package README (metadata-declared or by common names) and
/// Markdown under the package's docs/ directory.
pub fn discover(package: &PackageIdentity, readme: Option<&Path>) -> Vec<MarkdownFile> {
    let span = info_span!("markdown_discovery", package = %package.display());
    let _enter = span.enter();
    let root = package.root();
    let mut files = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let readme_candidates: Vec<PathBuf> = readme
        .map(|p| {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            }
        })
        .into_iter()
        .chain(
            ["README.md", "README.markdown", "README"]
                .into_iter()
                .map(|n| root.join(n)),
        )
        .collect();

    for candidate in readme_candidates {
        if candidate.is_file() && seen.insert(candidate.clone()) {
            files.push(MarkdownFile {
                rel_path: rel_name(&candidate, root),
                abs_path: candidate,
                kind: SourceKind::CrateReadme,
            });
            break; // one README per package; the first declared wins
        }
    }

    let docs_dir = root.join("docs");
    if docs_dir.is_dir() {
        let mut doc_files: Vec<PathBuf> = Vec::new();
        collect_markdown(&docs_dir, &mut doc_files);
        doc_files.sort(); // deterministic order
        for path in doc_files {
            if seen.insert(path.clone()) {
                files.push(MarkdownFile {
                    rel_path: rel_name(&path, root),
                    abs_path: path,
                    kind: SourceKind::MarkdownDocument,
                });
            }
        }
    }

    info!(files = files.len(), "discovered markdown");
    files
}

fn rel_name(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_ref()) {
                collect_markdown(&path, out);
            }
        } else if path.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown")
        }) {
            if let Ok(meta) = entry.metadata()
                && meta.len() <= MAX_FILE_BYTES
            {
                out.push(path);
            } else {
                debug!(path = %path.display(), "skipping oversized markdown file");
            }
        }
    }
}

/// Maximum accumulated text per chunk before splitting at a block boundary.
pub const MAX_CHUNK_CHARS: usize = 4000;

/// Chunks one Markdown file into documents with heading provenance.
pub fn chunk_markdown(
    package: &PackageIdentity,
    file: &MarkdownFile,
    text: &str,
) -> Vec<KnowledgeDocument> {
    let span = info_span!("markdown_ingestion", file = %file.rel_path);
    let _enter = span.enter();

    let mut chunker = Chunker {
        package,
        file,
        line_index: LineIndex::new(text),
        heading_stack: Vec::new(),
        collecting_heading: false,
        heading_text: String::new(),
        heading_level: HeadingLevel::H1,
        buf: String::new(),
        buf_start_offset: 0,
        section_ordinal: 0,
        chunk_ordinal: 0,
        docs: Vec::new(),
    };

    let parser = Parser::new_ext(text, Options::all());
    for (event, range) in parser.into_offset_iter() {
        chunker.on_event(event, range);
    }
    chunker.flush();

    info!(chunks = chunker.docs.len(), "chunked markdown file");
    chunker.docs
}

/// Byte offset to 1-based line number.
struct LineIndex {
    newline_offsets: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        LineIndex {
            newline_offsets: text
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i)
                .collect(),
        }
    }

    fn line(&self, offset: usize) -> u32 {
        let before = self.newline_offsets.partition_point(|&n| n < offset);
        u32::try_from(before + 1).unwrap_or(u32::MAX)
    }
}

struct Chunker<'a> {
    package: &'a PackageIdentity,
    file: &'a MarkdownFile,
    line_index: LineIndex,
    /// (level, title) heading ancestry of the current section.
    heading_stack: Vec<(HeadingLevel, String)>,
    /// While a heading's own text events are being collected.
    collecting_heading: bool,
    heading_text: String,
    heading_level: HeadingLevel,
    /// Accumulated content of the current chunk.
    buf: String,
    buf_start_offset: usize,
    /// Ordinal of the current section (increments on each heading start);
    /// 0 is the preamble before the first heading.
    section_ordinal: usize,
    /// Ordinal of the chunk within the current section.
    chunk_ordinal: usize,
    docs: Vec<KnowledgeDocument>,
}

impl Chunker<'_> {
    fn on_event(&mut self, event: Event, range: std::ops::Range<usize>) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                // A new heading ends the previous section's content.
                self.flush();
                self.collecting_heading = true;
                self.heading_text.clear();
                self.heading_level = level;
                self.section_ordinal += 1;
                self.chunk_ordinal = 0;
                self.buf.clear();
                self.buf_start_offset = range.end;
            }
            Event::End(TagEnd::Heading(_)) => {
                self.collecting_heading = false;
                let title = self.heading_text.trim().to_string();
                self.heading_text.clear();
                let level = self.heading_level;
                while matches!(self.heading_stack.last(), Some((l, _)) if *l >= level) {
                    self.heading_stack.pop();
                }
                self.heading_stack.push((level, title));
                self.buf.clear();
                self.buf_start_offset = range.end;
            }
            Event::Text(t) | Event::Code(t) => {
                if self.collecting_heading {
                    self.heading_text.push_str(&t);
                } else {
                    if self.buf.is_empty() {
                        self.buf_start_offset = range.start;
                    }
                    self.append_with_split(&t);
                }
            }
            Event::Start(Tag::Paragraph | Tag::List(_)) => {
                self.split_if_full(1);
            }
            Event::End(TagEnd::Paragraph | TagEnd::CodeBlock | TagEnd::List(_)) => {
                self.buf.push_str("\n\n");
            }
            Event::Start(Tag::CodeBlock(_)) => {
                self.split_if_full(1);
                self.buf.push('\n');
            }
            Event::Start(Tag::Item) => {
                if !self.buf.is_empty() {
                    self.buf.push('\n');
                }
                self.buf.push_str("- ");
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.collecting_heading {
                    self.heading_text.push(' ');
                } else {
                    self.append_with_split(" ");
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                // HTML carries noise; a single line is kept for context.
                self.append_with_split(&format!(
                    "{}
",
                    t.trim()
                ));
            }
            _ => {}
        }
    }

    /// Appends text, splitting the chunk at this boundary if it would
    /// overflow MAX_CHUNK_CHARS.
    ///
    /// A single event can itself be larger than MAX_CHUNK_CHARS (a
    /// pathological one-line paragraph, a huge inline-HTML block), and such
    /// an event can never fit whole: it is split at character boundaries so
    /// every chunk stays bounded and multibyte characters are never cut.
    fn append_with_split(&mut self, text: &str) {
        if text.len() <= MAX_CHUNK_CHARS {
            // Fast path — every realistic event: split between events only.
            self.split_if_full(text.len());
            self.buf.push_str(text);
            return;
        }
        // Pathological event: flush what is pending, then emit the event
        // in bounded pieces.
        if !self.buf.is_empty() {
            self.flush();
            self.chunk_ordinal += 1;
        }
        let mut piece = String::with_capacity(MAX_CHUNK_CHARS);
        for ch in text.chars() {
            if piece.len() + ch.len_utf8() > MAX_CHUNK_CHARS {
                self.buf.push_str(&piece);
                self.flush();
                self.chunk_ordinal += 1;
                piece.clear();
            }
            piece.push(ch);
        }
        self.buf.push_str(&piece);
    }

    fn split_if_full(&mut self, incoming: usize) {
        if !self.buf.is_empty() && self.buf.len() + incoming > MAX_CHUNK_CHARS {
            self.flush();
            self.chunk_ordinal += 1;
        }
    }

    fn flush(&mut self) {
        let body = self.buf.trim().to_string();
        self.buf.clear();
        if body.is_empty() {
            return;
        }

        let section_path: Vec<String> = self
            .heading_stack
            .iter()
            .map(|(_, title)| title.clone())
            .collect();

        let title = section_path
            .last()
            .cloned()
            .unwrap_or_else(|| file_title(&self.file.rel_path));

        let id = DocumentId::from_identity(&[
            &self.package.package_id,
            self.file.kind.as_str(),
            &self.file.rel_path,
            &self.section_ordinal.to_string(),
            &self.chunk_ordinal.to_string(),
        ]);

        let start_line = self.line_index.line(self.buf_start_offset);
        let end_line = self.line_index.line(self.buf_start_offset + body.len());

        self.docs.push(KnowledgeDocument {
            id,
            package: self.package.clone(),
            source_kind: self.file.kind,
            title,
            symbol_path: None,
            item_kind: None,
            section_path,
            text: body,
            source_path: Some(self.file.abs_path.clone()),
            source_span: Some(SourceSpan {
                start_line,
                start_col: 1,
                end_line,
                end_col: 1,
            }),
            related_symbols: Vec::new(),
            signature: None,
        });
    }
}

fn file_title(rel_path: &str) -> String {
    rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .trim_end_matches(".md")
        .trim_end_matches(".markdown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg() -> PackageIdentity {
        PackageIdentity {
            package_id: "test#pkg@1.0.0".into(),
            name: "pkg".into(),
            version: "1.0.0".into(),
            source: None,
            manifest_path: "/tmp/pkg/Cargo.toml".into(),
        }
    }

    fn md_file(rel: &str, kind: SourceKind) -> MarkdownFile {
        MarkdownFile {
            rel_path: rel.into(),
            abs_path: PathBuf::from(format!("/tmp/pkg/{rel}")),
            kind,
        }
    }

    fn chunks(text: &str) -> Vec<KnowledgeDocument> {
        chunk_markdown(&pkg(), &md_file("README.md", SourceKind::CrateReadme), text)
    }

    #[test]
    fn sections_retain_heading_ancestry() {
        let docs = chunks(
            "# Runtime\nintro text\n\n## CPU-bound work\noffload it.\n\n### details\nmore details.\n",
        );
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[0].section_path, vec!["Runtime"]);
        assert!(docs[0].text.contains("intro text"));
        assert_eq!(docs[1].section_path, vec!["Runtime", "CPU-bound work"]);
        assert!(docs[1].text.contains("offload it."));
        assert_eq!(
            docs[2].section_path,
            vec!["Runtime", "CPU-bound work", "details"]
        );
        assert_eq!(docs[2].title, "details");
    }

    #[test]
    fn preamble_is_its_own_chunk() {
        let docs = chunks("leading text before any heading\n\n# Section\nbody\n");
        assert_eq!(docs.len(), 2);
        assert!(docs[0].section_path.is_empty());
        assert_eq!(docs[0].title, "README");
        assert!(docs[0].text.contains("leading text"));
        assert_eq!(docs[1].section_path, vec!["Section"]);
    }

    #[test]
    fn sibling_headings_close_the_previous_section() {
        let docs = chunks("# A\ntext a\n\n# B\ntext b\n");
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1].section_path, vec!["B"]);
    }

    #[test]
    fn oversized_single_event_is_split_at_char_boundaries() {
        // One paragraph, one Text event, 6000 bytes of multibyte text:
        // larger than MAX_CHUNK_CHARS and impossible to cut on a block
        // boundary.
        let text = format!("# Big\n\n{}\n", "\u{e4}".repeat(3_000));
        let docs = chunks(&text);
        assert!(
            docs.len() >= 2,
            "expected a split, got {} chunks",
            docs.len()
        );
        for doc in &docs {
            assert!(
                doc.text.len() <= MAX_CHUNK_CHARS,
                "chunk is {} bytes",
                doc.text.len()
            );
            assert_eq!(doc.section_path, vec!["Big"]);
            assert!(
                doc.text.chars().all(|c| c == '\u{e4}'),
                "split mid-character"
            );
        }
        // Splitting is deterministic: same input, same ids.
        let again = chunks(&text);
        assert_eq!(docs, again);
    }

    #[test]
    fn oversized_ascii_event_is_split() {
        let text = format!("# Big\n\n{}\n", "a".repeat(9_000));
        let docs = chunks(&text);
        assert!(docs.len() >= 2, "got {} chunks", docs.len());
        for doc in &docs {
            assert!(
                doc.text.len() <= MAX_CHUNK_CHARS + 3,
                "chunk is {} bytes",
                doc.text.len()
            );
        }
        let ids: HashSet<&_> = docs.iter().map(|d| &d.id).collect();
        assert_eq!(ids.len(), docs.len(), "split chunks must have unique ids");
    }

    #[test]
    fn oversized_section_splits_with_ancestry_kept() {
        let para = "0123456789 ".repeat(30); // ~330 chars per paragraph
        let mut text = String::from("# Big\n\n");
        for _ in 0..30 {
            text.push_str(&para);
            text.push_str("\n\n");
        }
        let docs = chunks(&text);
        assert!(
            docs.len() > 1,
            "expected multiple chunks, got {}",
            docs.len()
        );
        for doc in &docs {
            assert_eq!(doc.section_path, vec!["Big"]);
        }
        // Deterministic split: same input, same ids.
        let docs2 = chunks(&text);
        assert_eq!(docs, docs2);
    }

    #[test]
    fn ids_are_stable_across_rebuilds() {
        let a = chunks("# A\nbody\n");
        let b = chunks("# A\nbody\n");
        assert_eq!(a[0].id, b[0].id);
    }

    #[test]
    fn code_blocks_are_kept() {
        let docs = chunks("# Usage\n\n```rust\nlet w = Writer::new();\n```\n");
        assert_eq!(docs.len(), 1);
        assert!(docs[0].text.contains("let w = Writer::new();"));
    }

    #[test]
    fn heading_level_skips_are_handled() {
        let docs = chunks("# A\n\ntext\n\n### Deep\n\nmore\n");
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1].section_path, vec!["A", "Deep"]);
    }

    #[test]
    fn file_title_is_stem() {
        assert_eq!(file_title("docs/guide.md"), "guide");
        assert_eq!(file_title("README.markdown"), "README");
    }
}
