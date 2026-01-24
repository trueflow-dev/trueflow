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
    #[serde(rename = "Signature")]
    Signature,
    #[serde(rename = "test")]
    Test,
}

impl BlockKind {
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
            BlockKind::Signature => "Signature",
            BlockKind::Test => "test",
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
            "signature" => BlockKind::Signature,
            "test" => BlockKind::Test,
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

    /// 0-indexed start line (inclusive)
    pub start_line: usize,

    /// 0-indexed end line (exclusive)
    pub end_line: usize,
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
