use crate::analysis::Language;
use crate::hashing::{BytesHash, TreeHash};
use crate::repo_path::RepoPath;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum BlockKind {
    #[serde(rename = "TextBlock")]
    #[default]
    TextBlock,
    #[serde(rename = "code")]
    Code,
    #[serde(rename = "gap", alias = "Gap")]
    Gap,
    #[serde(rename = "comment")]
    Comment,
    #[serde(rename = "Section")]
    Section,
    #[serde(rename = "Preamble")]
    Preamble,
    #[serde(rename = "function")]
    Function,
    #[serde(rename = "struct")]
    Struct,
    #[serde(rename = "enum")]
    Enum,
    #[serde(rename = "impl")]
    Impl,
    #[serde(rename = "module")]
    Module,
    #[serde(rename = "Modules")]
    Modules,
    #[serde(rename = "import")]
    Import,
    #[serde(rename = "const")]
    Const,
    #[serde(rename = "static")]
    Static,
    #[serde(rename = "macro")]
    Macro,
    #[serde(rename = "class")]
    Class,
    #[serde(rename = "export")]
    Export,
    #[serde(rename = "variable")]
    Variable,
    #[serde(rename = "decorator")]
    Decorator,
    #[serde(rename = "interface")]
    Interface,
    #[serde(rename = "type")]
    Type,
    #[serde(rename = "method")]
    Method,
    #[serde(rename = "command")]
    Command,
    #[serde(rename = "CodeParagraph")]
    CodeParagraph,
    #[serde(rename = "Header")]
    Header,
    #[serde(rename = "Paragraph")]
    Paragraph,
    #[serde(rename = "CodeBlock")]
    CodeBlock,
    #[serde(rename = "List")]
    List,
    #[serde(rename = "ListItem")]
    ListItem,
    #[serde(rename = "Quote")]
    Quote,
    #[serde(rename = "Element")]
    Element,
    #[serde(rename = "Content")]
    Content,
    #[serde(rename = "Sentence")]
    Sentence,
    #[serde(rename = "Imports")]
    Imports,
    #[serde(rename = "FunctionSignature")]
    FunctionSignature,
}

impl BlockKind {
    pub fn is_import_like(self) -> bool {
        matches!(
            self,
            BlockKind::Import | BlockKind::Imports | BlockKind::Module | BlockKind::Modules
        )
    }

    pub fn can_contain_review_children(self) -> bool {
        matches!(
            self,
            BlockKind::Impl
                | BlockKind::Interface
                | BlockKind::Class
                | BlockKind::Struct
                | BlockKind::Enum
                | BlockKind::Section
        )
    }

    pub fn default_review_priority(self) -> u8 {
        if self.is_import_like() {
            return 70;
        }

        match self {
            BlockKind::Struct
            | BlockKind::Enum
            | BlockKind::Type
            | BlockKind::Interface
            | BlockKind::Class => 0,

            BlockKind::Const | BlockKind::Static => 20,
            BlockKind::FunctionSignature => 30,
            BlockKind::Impl => 40,
            BlockKind::Function | BlockKind::Method => 50,

            BlockKind::Gap | BlockKind::Comment => 95,

            _ => 60,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::TextBlock => "TextBlock",
            BlockKind::Code => "code",
            BlockKind::Gap => "gap",
            BlockKind::Comment => "comment",
            BlockKind::Section => "Section",
            BlockKind::Preamble => "Preamble",
            BlockKind::Function => "function",
            BlockKind::Struct => "struct",
            BlockKind::Enum => "enum",
            BlockKind::Impl => "impl",
            BlockKind::Module => "module",
            BlockKind::Modules => "Modules",
            BlockKind::Import => "import",
            BlockKind::Const => "const",
            BlockKind::Static => "static",
            BlockKind::Macro => "macro",
            BlockKind::Class => "class",
            BlockKind::Export => "export",
            BlockKind::Variable => "variable",
            BlockKind::Decorator => "decorator",
            BlockKind::Interface => "interface",
            BlockKind::Type => "type",
            BlockKind::Method => "method",
            BlockKind::Command => "command",
            BlockKind::CodeParagraph => "CodeParagraph",
            BlockKind::Header => "Header",
            BlockKind::Paragraph => "Paragraph",
            BlockKind::CodeBlock => "CodeBlock",
            BlockKind::List => "List",
            BlockKind::ListItem => "ListItem",
            BlockKind::Quote => "Quote",
            BlockKind::Element => "Element",
            BlockKind::Content => "Content",
            BlockKind::Sentence => "Sentence",
            BlockKind::Imports => "Imports",
            BlockKind::FunctionSignature => "FunctionSignature",
        }
    }
}

impl fmt::Display for BlockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn normalize_kind_name(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['_', '-'], "")
}

impl FromStr for BlockKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = normalize_kind_name(value);
        let kind = match normalized.as_str() {
            "textblock" => BlockKind::TextBlock,
            "code" => BlockKind::Code,
            "gap" | "whitespace" => BlockKind::Gap,
            "comment" => BlockKind::Comment,
            "section" => BlockKind::Section,
            "preamble" => BlockKind::Preamble,
            "function" => BlockKind::Function,
            "struct" => BlockKind::Struct,
            "enum" => BlockKind::Enum,
            "impl" => BlockKind::Impl,
            "module" => BlockKind::Module,
            "modules" => BlockKind::Modules,
            "import" => BlockKind::Import,
            "const" => BlockKind::Const,
            "static" => BlockKind::Static,
            "macro" => BlockKind::Macro,
            "class" => BlockKind::Class,
            "export" => BlockKind::Export,
            "variable" => BlockKind::Variable,
            "decorator" => BlockKind::Decorator,
            "interface" => BlockKind::Interface,
            "type" => BlockKind::Type,
            "method" => BlockKind::Method,
            "command" => BlockKind::Command,
            "codeparagraph" => BlockKind::CodeParagraph,
            "header" => BlockKind::Header,
            "paragraph" => BlockKind::Paragraph,
            "codeblock" => BlockKind::CodeBlock,
            "list" => BlockKind::List,
            "listitem" => BlockKind::ListItem,
            "quote" => BlockKind::Quote,
            "element" => BlockKind::Element,
            "content" => BlockKind::Content,
            "sentence" => BlockKind::Sentence,
            "imports" => BlockKind::Imports,
            "functionsignature" | "signature" => BlockKind::FunctionSignature,
            _ => {
                return Err(anyhow!("Unknown block kind: {value}"));
            }
        };

        Ok(kind)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    /// Content fingerprint for this source occurrence. It deliberately does not
    /// include the path or source position.
    pub hash: TreeHash,

    /// The exact UTF-8 source text named by `start_byte..end_byte`.
    pub content: String,

    /// Semantic type (Function, Struct, Comment, Chunk, etc.)
    #[serde(default)]
    pub kind: BlockKind,

    /// Optional tags applied to this block (e.g. "test")
    #[serde(default)]
    pub tags: Vec<String>,

    /// Complexity score for this exact block when explicitly computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<u32>,

    /// 0-indexed start line (inclusive).
    pub start_line: usize,

    /// 0-indexed end line (exclusive).
    pub end_line: usize,

    /// Absolute UTF-8 byte offset into the source file (inclusive).
    pub start_byte: usize,

    /// Absolute UTF-8 byte offset into the source file (exclusive).
    pub end_byte: usize,
}

impl Block {
    /// Builds a block with explicit source coordinates. Prefer
    /// `from_file_range` or `from_parent_range` whenever source text is
    /// available so the content and line coordinates are proved.
    pub fn new(content: String, kind: BlockKind, line_span: LineSpan, byte_span: ByteSpan) -> Self {
        assert!(
            line_span.start_line <= line_span.end_line,
            "block line span must be ordered"
        );
        assert!(
            byte_span.start_byte <= byte_span.end_byte,
            "block byte span must be ordered"
        );
        assert_eq!(
            content.len(),
            byte_span.len(),
            "block content byte length must equal its source span length"
        );

        Self {
            hash: TreeHash::from_content(&content),
            content,
            kind,
            tags: Vec::new(),
            complexity: None,
            start_line: line_span.start_line,
            end_line: line_span.end_line,
            start_byte: byte_span.start_byte,
            end_byte: byte_span.end_byte,
        }
    }

    /// Constructs a block from an absolute byte range in its complete source.
    ///
    /// The range is checked for ordering, bounds, and UTF-8 boundaries before
    /// content and line coordinates are derived from the same source slice.
    pub(crate) fn from_file_range(
        full_source: &str,
        kind: BlockKind,
        byte_span: ByteSpan,
    ) -> anyhow::Result<Self> {
        let content = full_source
            .get(byte_span.start_byte..byte_span.end_byte)
            .ok_or_else(|| {
                anyhow!(
                    "invalid source byte range {}..{} for {} bytes",
                    byte_span.start_byte,
                    byte_span.end_byte,
                    full_source.len()
                )
            })?;
        let line_span = Self::line_span_from_file_range(full_source, byte_span)?;

        Ok(Self::new(content.to_owned(), kind, line_span, byte_span))
    }

    /// Derives a line span from a checked absolute UTF-8 byte range without
    /// allocating the corresponding source slice.
    pub(crate) fn line_span_from_file_range(
        full_source: &str,
        byte_span: ByteSpan,
    ) -> anyhow::Result<LineSpan> {
        line_span_for_source_range(full_source, byte_span).ok_or_else(|| {
            anyhow!(
                "invalid source byte range {}..{} for {} bytes",
                byte_span.start_byte,
                byte_span.end_byte,
                full_source.len()
            )
        })
    }

    /// Constructs a block from a range relative to an exact source-backed
    /// parent, translating the stored coordinate to the original file.
    pub(crate) fn from_parent_range(
        parent: &Block,
        kind: BlockKind,
        relative_span: ByteSpan,
    ) -> anyhow::Result<Self> {
        if parent.content.len() != parent.byte_span().len() {
            return Err(anyhow!(
                "parent content byte length does not match its source span"
            ));
        }

        let content = parent
            .content
            .get(relative_span.start_byte..relative_span.end_byte)
            .ok_or_else(|| {
                anyhow!(
                    "invalid parent-relative byte range {}..{} for {} bytes",
                    relative_span.start_byte,
                    relative_span.end_byte,
                    parent.content.len()
                )
            })?;
        let start_byte = parent
            .start_byte
            .checked_add(relative_span.start_byte)
            .ok_or_else(|| anyhow!("parent-relative start byte overflow"))?;
        let end_byte = parent
            .start_byte
            .checked_add(relative_span.end_byte)
            .ok_or_else(|| anyhow!("parent-relative end byte overflow"))?;
        let byte_span = ByteSpan::new(start_byte, end_byte);
        if byte_span.end_byte > parent.end_byte {
            return Err(anyhow!(
                "parent-relative byte range {}..{} escapes parent {}..{}",
                relative_span.start_byte,
                relative_span.end_byte,
                parent.start_byte,
                parent.end_byte
            ));
        }

        let relative_line_span = Self::line_span_from_file_range(&parent.content, relative_span)?;
        let line_span = LineSpan::new(
            parent
                .start_line
                .checked_add(relative_line_span.start_line)
                .ok_or_else(|| anyhow!("parent-relative start line overflow"))?,
            parent
                .start_line
                .checked_add(relative_line_span.end_line)
                .ok_or_else(|| anyhow!("parent-relative end line overflow"))?,
        );

        Ok(Self::new(content.to_owned(), kind, line_span, byte_span))
    }

    pub fn line_span(&self) -> LineSpan {
        LineSpan::new(self.start_line, self.end_line)
    }

    pub fn byte_span(&self) -> ByteSpan {
        ByteSpan::new(self.start_byte, self.end_byte)
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|candidate| candidate == tag)
    }

    pub fn is_test(&self) -> bool {
        self.has_tag("test")
    }
}

fn line_span_for_source_range(source: &str, byte_span: ByteSpan) -> Option<LineSpan> {
    let before = source.get(..byte_span.start_byte)?;
    let content = source.get(byte_span.start_byte..byte_span.end_byte)?;
    let start_line = before.chars().filter(|&ch| ch == '\n').count();
    if content.is_empty() {
        return Some(LineSpan::new(start_line, start_line));
    }

    let end_line = start_line
        .checked_add(content.chars().filter(|&ch| ch == '\n').count())?
        .checked_add(usize::from(!content.ends_with('\n')))?;
    Some(LineSpan::new(start_line, end_line))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    pub start_line: usize,
    pub end_line: usize,
}

impl LineSpan {
    pub fn new(start_line: usize, end_line: usize) -> Self {
        Self {
            start_line,
            end_line,
        }
    }

    pub fn len(&self) -> usize {
        self.end_line.saturating_sub(self.start_line)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn overlaps(&self, other: &LineSpan) -> bool {
        self.start_line < other.end_line && self.end_line > other.start_line
    }
}

#[cfg(test)]
impl LineSpan {
    pub fn contains(&self, other: &LineSpan) -> bool {
        self.start_line <= other.start_line && self.end_line >= other.end_line
    }
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord, Default,
)]
pub struct ByteSpan {
    pub start_byte: usize,
    pub end_byte: usize,
}

impl ByteSpan {
    pub fn new(start_byte: usize, end_byte: usize) -> Self {
        Self {
            start_byte,
            end_byte,
        }
    }

    pub fn len(&self) -> usize {
        self.end_byte.saturating_sub(self.start_byte)
    }

    pub fn is_empty(&self) -> bool {
        self.start_byte == self.end_byte
    }

    pub fn overlaps(&self, other: &ByteSpan) -> bool {
        self.start_byte < other.end_byte && self.end_byte > other.start_byte
    }

    pub fn contains(&self, other: &ByteSpan) -> bool {
        self.start_byte <= other.start_byte && self.end_byte >= other.end_byte
    }

    pub fn properly_contains(&self, other: &ByteSpan) -> bool {
        self.contains(other) && self != other
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub path: RepoPath,
    #[serde(default)]
    pub language: Language,
    /// Exact hash of the raw file bytes.
    pub bytes_hash: BytesHash,
    /// Review-tree hash for this file. For files with blocks, this is the ordered
    /// hash of child block hashes. For leaf-like files with no blocks, this reuses
    /// the bytes hash.
    pub tree_hash: TreeHash,
    pub blocks: Vec<Block>,
}

impl FileState {
    pub fn new(
        path: RepoPath,
        language: Language,
        bytes_hash: BytesHash,
        mut blocks: Vec<Block>,
    ) -> Self {
        assert!(!path.is_root(), "FileState path must not be root");
        blocks.sort_by_key(|block| {
            (
                block.start_byte,
                Reverse(block.end_byte),
                block.start_line,
                Reverse(block.end_line),
            )
        });
        let tree_hash = if blocks.is_empty() {
            TreeHash::from_bytes_hash(&bytes_hash)
        } else {
            TreeHash::from_child_hashes(blocks.iter().map(|block| &block.hash))
        };

        Self {
            path,
            language,
            bytes_hash,
            tree_hash,
            blocks,
        }
    }

    pub fn from_text(path: RepoPath, language: Language, bytes: &[u8], blocks: Vec<Block>) -> Self {
        Self::new(path, language, BytesHash::from_bytes(bytes), blocks)
    }

    pub fn from_binary(path: RepoPath, bytes: &[u8]) -> Self {
        Self::new(
            path,
            Language::Unknown,
            BytesHash::from_bytes(bytes),
            Vec::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_kind_serialization_round_trip() {
        // List all variants to ensure full coverage
        let kinds = [
            BlockKind::TextBlock,
            BlockKind::Code,
            BlockKind::Gap,
            BlockKind::Comment,
            BlockKind::Section,
            BlockKind::Preamble,
            BlockKind::Function,
            BlockKind::Struct,
            BlockKind::Enum,
            BlockKind::Impl,
            BlockKind::Module,
            BlockKind::Modules,
            BlockKind::Import,
            BlockKind::Const,
            BlockKind::Static,
            BlockKind::Macro,
            BlockKind::Class,
            BlockKind::Export,
            BlockKind::Variable,
            BlockKind::Decorator,
            BlockKind::Interface,
            BlockKind::Type,
            BlockKind::Method,
            BlockKind::Command,
            BlockKind::CodeParagraph,
            BlockKind::Header,
            BlockKind::Paragraph,
            BlockKind::CodeBlock,
            BlockKind::List,
            BlockKind::ListItem,
            BlockKind::Quote,
            BlockKind::Element,
            BlockKind::Content,
            BlockKind::Sentence,
            BlockKind::Imports,
            BlockKind::FunctionSignature,
        ];

        for kind in kinds {
            // 1. Test as_str()
            let s = kind.as_str();
            assert!(!s.is_empty(), "as_str() returned empty string for {kind:?}");

            // 2. Test Display
            let display_str = format!("{kind}");
            assert_eq!(display_str, s, "Display impl mismatch for {kind:?}");

            // 3. Test FromStr (exact match)
            let parsed = BlockKind::from_str(s).unwrap();
            assert_eq!(parsed, kind, "FromStr roundtrip failed for {kind:?}");

            // 4. Test FromStr (case insensitive normalization)
            let upper = s.to_uppercase();
            let parsed_upper = BlockKind::from_str(&upper).unwrap();
            assert_eq!(
                parsed_upper, kind,
                "FromStr uppercase roundtrip failed for {kind:?}"
            );
        }
    }

    #[test]
    fn test_block_kind_normalization_edge_cases() {
        assert_eq!(
            BlockKind::from_str("code-block").unwrap(),
            BlockKind::CodeBlock
        );
        assert_eq!(
            BlockKind::from_str("list_item").unwrap(),
            BlockKind::ListItem
        );
        assert_eq!(
            BlockKind::from_str("textblock").unwrap(),
            BlockKind::TextBlock
        );
        assert_eq!(BlockKind::from_str("whitespace").unwrap(), BlockKind::Gap);
        assert_eq!(BlockKind::from_str("code").unwrap(), BlockKind::Code);
    }

    #[test]
    fn test_block_helpers_report_line_span_byte_span_and_test_tag() {
        let mut block = Block::new(
            "fn test_thing() {}".to_string(),
            BlockKind::Function,
            LineSpan::new(3, 4),
            ByteSpan::new(40, 58),
        );
        block.tags.push("test".to_string());
        block.tags.push("integration".to_string());

        assert_eq!(block.line_span(), LineSpan::new(3, 4));
        assert_eq!(block.byte_span(), ByteSpan::new(40, 58));
        assert!(block.has_tag("test"));
        assert!(block.has_tag("integration"));
        assert!(block.is_test());
        assert!(!block.has_tag("unit"));
    }

    #[test]
    fn test_line_span_overlap_logic() {
        let base = LineSpan::new(0, 10);
        let overlap = LineSpan::new(5, 12);
        let touch = LineSpan::new(10, 12);
        let disjoint = LineSpan::new(12, 15);

        assert_eq!(base.len(), 10);
        assert!(base.overlaps(&overlap));
        assert!(!base.overlaps(&touch));
        assert!(!base.overlaps(&disjoint));
        assert!(base.contains(&LineSpan::new(0, 10)));
        assert!(base.contains(&LineSpan::new(2, 5)));
        assert!(!base.contains(&overlap));
    }

    #[test]
    fn test_byte_span_boundary_logic() {
        let base = ByteSpan::new(0, 10);
        let overlap = ByteSpan::new(5, 12);
        let touch = ByteSpan::new(10, 12);
        let disjoint = ByteSpan::new(12, 15);
        let empty = ByteSpan::new(5, 5);

        assert_eq!(base.len(), 10);
        assert!(base.overlaps(&overlap));
        assert!(!base.overlaps(&touch));
        assert!(!base.overlaps(&disjoint));
        assert!(empty.is_empty());
        assert!(base.contains(&ByteSpan::new(0, 10)));
        assert!(base.contains(&ByteSpan::new(2, 5)));
        assert!(base.contains(&empty));
        assert!(!base.contains(&overlap));
        assert!(base.properly_contains(&ByteSpan::new(2, 5)));
        assert!(!base.properly_contains(&ByteSpan::new(0, 10)));
    }

    #[test]
    fn test_file_state_tracks_bytes_hash_and_tree_hash() {
        let path = crate::repo_path::RepoPath::new("src/lib.rs").unwrap();
        let blocks = vec![
            Block::new(
                "fn b() {}\n".to_string(),
                BlockKind::Function,
                LineSpan::new(1, 2),
                ByteSpan::new(10, 20),
            ),
            Block::new(
                "fn a() {}\n".to_string(),
                BlockKind::Function,
                LineSpan::new(0, 1),
                ByteSpan::new(0, 10),
            ),
        ];
        let file = FileState::from_text(path, Language::Rust, b"fn a() {}\nfn b() {}\n", blocks);

        assert_eq!(file.path.as_str(), "src/lib.rs");
        assert_eq!(file.blocks[0].start_byte, 0);
        assert_eq!(file.blocks[1].start_byte, 10);
        assert_eq!(
            file.tree_hash,
            crate::hashing::TreeHash::from_child_hashes(
                file.blocks.iter().map(|block| &block.hash)
            )
        );
        assert_ne!(file.bytes_hash.as_str(), file.tree_hash.as_str());
    }

    #[test]
    fn test_source_range_constructors_validate_utf8_and_translate_parent_offsets() {
        let source = "é\nfn outer() {\n    let β = 1;\n}\n";
        let parent_start = source.find("fn outer").unwrap();
        let parent = Block::from_file_range(
            source,
            BlockKind::Function,
            ByteSpan::new(parent_start, source.len()),
        )
        .unwrap();
        let child_start = parent.content.find("let β").unwrap();
        let child_end = child_start + "let β = 1;".len();
        let child = Block::from_parent_range(
            &parent,
            BlockKind::CodeParagraph,
            ByteSpan::new(child_start, child_end),
        )
        .unwrap();

        assert_eq!(&source[child.start_byte..child.end_byte], "let β = 1;");
        assert_eq!(child.line_span(), LineSpan::new(2, 3));
        assert_eq!(child.byte_span().start_byte, parent_start + child_start);
        assert!(Block::from_file_range(source, BlockKind::Code, ByteSpan::new(1, 2)).is_err());
        assert!(
            Block::from_parent_range(
                &parent,
                BlockKind::Code,
                ByteSpan::new(parent.content.len(), parent.content.len() + 1),
            )
            .is_err()
        );
    }

    #[test]
    fn test_block_serialization_includes_required_byte_span() {
        let block = Block::new(
            "fn a() {}".to_string(),
            BlockKind::Function,
            LineSpan::new(0, 1),
            ByteSpan::new(0, 9),
        );
        let value = serde_json::to_value(&block).unwrap();

        assert_eq!(value["start_byte"], 0);
        assert_eq!(value["end_byte"], 9);
        assert!(value.get("complexity").is_none());
    }
}
