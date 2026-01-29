use crate::analysis::Language;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
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
    pub fn is_import_like(&self) -> bool {
        matches!(
            self,
            BlockKind::Import | BlockKind::Imports | BlockKind::Module | BlockKind::Modules
        )
    }

    pub fn default_review_priority(&self) -> u8 {
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

    pub fn as_str(&self) -> &'static str {
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
                return Err(anyhow!("Unknown block kind: {}", value));
            }
        };

        Ok(kind)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    /// The content-addressable identity of this block
    pub hash: String,

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
            hash: crate::hashing::hash_str(&content),
            content,
            kind,
            tags: Vec::new(),
            complexity: 0,
            start_line,
            end_line,
        }
    }

    pub fn span(&self) -> Span {
        Span::new(self.start_line, self.end_line)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn overlaps(&self, other: &Span) -> bool {
        self.start < other.end && self.end > other.start
    }

    pub fn contains(&self, other: &Span) -> bool {
        self.start <= other.start && self.end >= other.end
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    #[serde(default)]
    pub language: Language,
    /// The hash of the entire file (e.g. Merkle root of blocks)
    pub file_hash: String,
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
            assert!(
                !s.is_empty(),
                "as_str() returned empty string for {:?}",
                kind
            );

            // 2. Test Display
            let display_str = format!("{}", kind);
            assert_eq!(display_str, s, "Display impl mismatch for {:?}", kind);

            // 3. Test FromStr (exact match)
            let parsed = BlockKind::from_str(s).expect("Failed to parse back from as_str()");
            assert_eq!(parsed, kind, "FromStr roundtrip failed for {:?}", kind);

            // 4. Test FromStr (case insensitive normalization)
            let upper = s.to_uppercase();
            let parsed_upper = BlockKind::from_str(&upper).expect("Failed to parse uppercase");
            assert_eq!(
                parsed_upper, kind,
                "FromStr uppercase roundtrip failed for {:?}",
                kind
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
    fn test_span_overlap_logic() {
        let base = Span::new(0, 10);
        let overlap = Span::new(5, 12);
        let touch = Span::new(10, 12);
        let disjoint = Span::new(12, 15);

        assert!(base.overlaps(&overlap));
        assert!(!base.overlaps(&touch));
        assert!(!base.overlaps(&disjoint));
        assert!(base.contains(&Span::new(0, 10)));
        assert!(base.contains(&Span::new(2, 5)));
        assert!(!base.contains(&overlap));
    }
}
