use crate::analysis::Language;
use crate::hashing::ContentHash;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
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
            "gap" => BlockKind::Gap,
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
    /// The content-addressable identity of this block
    pub hash: ContentHash,

    /// The actual text content
    pub content: String,

    /// Semantic type (Function, Struct, Comment, Chunk, etc.)
    #[serde(default)]
    pub kind: BlockKind,

    /// Optional tags applied to this block (e.g. "test")
    #[serde(default)]
    pub tags: Vec<String>,

    /// Optional complexity score
    #[serde(default)]
    pub complexity: u32,

    /// 0-indexed start line (inclusive)
    pub start_line: usize,

    /// 0-indexed end line (exclusive)
    pub end_line: usize,
}

impl Block {
    pub fn new(content: String, kind: BlockKind, start_line: usize, end_line: usize) -> Self {
        Self {
            hash: ContentHash::from_content(&content),
            content,
            kind,
            tags: Vec::new(),
            complexity: 0,
            start_line,
            end_line,
        }
    }

    pub fn line_span(&self) -> LineSpan {
        LineSpan::new(self.start_line, self.end_line)
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|candidate| candidate == tag)
    }

    pub fn is_test(&self) -> bool {
        self.has_tag("test")
    }
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

    pub fn overlaps(&self, other: &LineSpan) -> bool {
        self.start_line < other.end_line && self.end_line > other.start_line
    }

    pub fn contains(&self, other: &LineSpan) -> bool {
        self.start_line <= other.start_line && self.end_line >= other.end_line
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    pub fn overlaps(&self, other: &ByteSpan) -> bool {
        self.start_byte < other.end_byte && self.end_byte > other.start_byte
    }

    #[allow(dead_code)]
    pub fn contains(&self, other: &ByteSpan) -> bool {
        self.start_byte <= other.start_byte && self.end_byte >= other.end_byte
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    #[serde(default)]
    pub language: Language,
    /// The hash of the entire file (e.g. Merkle root of blocks)
    pub file_hash: ContentHash,
    pub blocks: Vec<Block>,
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
        assert_eq!(BlockKind::from_str("code").unwrap(), BlockKind::Code);
    }

    #[test]
    fn test_block_helpers_report_line_span_and_test_tag() {
        let mut block = Block::new("fn test_thing() {}".to_string(), BlockKind::Function, 3, 7);
        block.tags.push("test".to_string());
        block.tags.push("integration".to_string());

        assert_eq!(block.line_span(), LineSpan::new(3, 7));
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

        assert!(base.overlaps(&overlap));
        assert!(!base.overlaps(&touch));
        assert!(!base.overlaps(&disjoint));
        assert!(base.contains(&LineSpan::new(0, 10)));
        assert!(base.contains(&LineSpan::new(2, 5)));
        assert!(!base.contains(&overlap));
    }

    #[test]
    fn test_byte_span_overlap_logic() {
        let base = ByteSpan::new(0, 10);
        let overlap = ByteSpan::new(5, 12);
        let touch = ByteSpan::new(10, 12);
        let disjoint = ByteSpan::new(12, 15);

        assert!(base.overlaps(&overlap));
        assert!(!base.overlaps(&touch));
        assert!(!base.overlaps(&disjoint));
        assert!(base.contains(&ByteSpan::new(0, 10)));
        assert!(base.contains(&ByteSpan::new(2, 5)));
        assert!(!base.contains(&overlap));
    }
}
