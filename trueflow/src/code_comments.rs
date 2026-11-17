use crate::analysis::Language;

/// The comment markers a language uses, which decides whether a line that
/// starts with `#` is a comment or code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSyntax {
    /// `//` and `/* */` only. A leading `#` is code: preprocessor
    /// directives in C, C++ and C#, attributes in Rust.
    CStyle,
    /// `#` line comments as well as C-style ones, for languages where `#`
    /// starts a comment or where neither marker is code.
    CStyleOrHash,
}

impl CommentSyntax {
    pub fn of(language: Language) -> Self {
        match language {
            Language::Rust
            | Language::Swift
            | Language::JavaScript
            | Language::TypeScript
            | Language::Java
            | Language::Kotlin
            | Language::CSharp
            | Language::Go
            | Language::C
            | Language::Cpp
            | Language::Zig
            | Language::Dart
            | Language::Scala
            | Language::Css
            | Language::Json => Self::CStyle,
            Language::Elisp
            | Language::Python
            | Language::Ruby
            | Language::Php
            | Language::Lua
            | Language::Haskell
            | Language::OCaml
            | Language::Elixir
            | Language::Clojure
            | Language::Sql
            | Language::Yaml
            | Language::Html
            | Language::Shell
            | Language::Markdown
            | Language::Toml
            | Language::Nix
            | Language::Just
            | Language::Text
            | Language::Unknown => Self::CStyleOrHash,
        }
    }

    fn starts_line_comment(self, trimmed_line: &str) -> bool {
        trimmed_line.starts_with("//")
            || match self {
                Self::CStyle => false,
                Self::CStyleOrHash => trimmed_line.starts_with('#'),
            }
    }
}

/// Tracks whether sequential lines or paragraph chunks are currently inside an
/// open `/* ... */` block comment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockCommentTracker {
    in_block_comment: bool,
}

impl BlockCommentTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classifies `chunk` while advancing `/* ... */` block-comment state across
    /// every line of `chunk` so subsequent paragraph chunks see accurate state.
    pub fn chunk_is_comment_only(&mut self, chunk: &str, syntax: CommentSyntax) -> bool {
        let mut saw_comment = false;
        let mut saw_code = false;

        for source_line in chunk.lines() {
            let (line_saw_comment, line_saw_code) = self.scan_line(source_line, syntax);
            saw_comment |= line_saw_comment;
            saw_code |= line_saw_code;
        }

        saw_comment && !saw_code
    }

    fn scan_line(&mut self, source_line: &str, syntax: CommentSyntax) -> (bool, bool) {
        let mut saw_comment = false;
        let mut saw_code = false;
        let mut rest = source_line;

        loop {
            if self.in_block_comment {
                saw_comment = true;
                if let Some(end) = rest.find("*/") {
                    rest = &rest[end + 2..];
                    self.in_block_comment = false;
                    continue;
                }
                break;
            }

            let trimmed = rest.trim_start();
            if trimmed.is_empty() {
                break;
            }
            if syntax.starts_line_comment(trimmed) {
                saw_comment = true;
                break;
            }
            if let Some(after_open) = trimmed.strip_prefix("/*") {
                saw_comment = true;
                self.in_block_comment = true;
                rest = after_open;
                continue;
            }

            saw_code = true;
            match advance_code_until_comment(trimmed, syntax) {
                CodeAdvance::EndOfLine => break,
                CodeAdvance::LineComment => {
                    saw_comment = true;
                    break;
                }
                CodeAdvance::BlockCommentOpen(after_open) => {
                    saw_comment = true;
                    self.in_block_comment = true;
                    rest = after_open;
                }
            }
        }

        (saw_comment, saw_code)
    }
}

enum CodeAdvance<'a> {
    EndOfLine,
    LineComment,
    BlockCommentOpen(&'a str),
}

fn advance_code_until_comment(code: &str, syntax: CommentSyntax) -> CodeAdvance<'_> {
    let bytes = code.as_bytes();
    let mut idx = 0;
    let mut quote: Option<u8> = None;

    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = quote {
            if b == b'\\' {
                idx = (idx + 2).min(bytes.len());
                continue;
            }
            if b == q {
                quote = None;
            }
            idx += 1;
            continue;
        }

        match b {
            b'"' | b'\'' => {
                quote = Some(b);
                idx += 1;
            }
            b'/' if bytes.get(idx + 1) == Some(&b'/') => return CodeAdvance::LineComment,
            b'/' if bytes.get(idx + 1) == Some(&b'*') => {
                return CodeAdvance::BlockCommentOpen(&code[idx + 2..]);
            }
            b'#' if matches!(syntax, CommentSyntax::CStyleOrHash) => {
                return CodeAdvance::LineComment;
            }
            _ => {
                idx += 1;
            }
        }
    }

    CodeAdvance::EndOfLine
}

pub fn chunk_is_comment_only(chunk: &str, syntax: CommentSyntax) -> bool {
    BlockCommentTracker::new().chunk_is_comment_only(chunk, syntax)
}

/// Byte length of leading blank/comment-only lines before the first line of code
/// in `chunk`, using C-style (`//` and `/* ... */`) comment rules.
pub fn leading_c_style_comment_prefix_len(chunk: &str) -> Option<usize> {
    let mut tracker = BlockCommentTracker::new();
    let mut offset = 0;
    let mut saw_comment = false;

    for line in chunk.split_inclusive('\n') {
        if line.trim().is_empty() && !tracker.in_block_comment {
            offset += line.len();
            continue;
        }
        let (line_saw_comment, line_saw_code) = tracker.scan_line(line, CommentSyntax::CStyle);
        if line_saw_code {
            return saw_comment.then_some(offset);
        }
        if line_saw_comment {
            saw_comment = true;
            offset += line.len();
            continue;
        }
        return saw_comment.then_some(offset);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_is_comment_only_accepts_comment_lines() {
        assert!(chunk_is_comment_only(
            "# one\n// two\n/* three\n* four\n*/",
            CommentSyntax::CStyleOrHash
        ));
    }

    #[test]
    fn chunk_is_comment_only_rejects_code_lines() {
        assert!(!chunk_is_comment_only(
            "// note\nlet value = 1;",
            CommentSyntax::CStyleOrHash
        ));
    }

    #[test]
    fn chunk_is_comment_only_rejects_code_after_block_comment() {
        assert!(!chunk_is_comment_only(
            "/* rationale */ dangerous_call();",
            CommentSyntax::CStyleOrHash
        ));
    }

    #[test]
    fn chunk_is_comment_only_rejects_pointer_dereference_lines() {
        for syntax in [CommentSyntax::CStyle, CommentSyntax::CStyleOrHash] {
            assert!(!chunk_is_comment_only("*out = value;", syntax));
            assert!(!chunk_is_comment_only("**slot = value;", syntax));
        }
        assert_eq!(leading_c_style_comment_prefix_len("*out = value;\n"), None);
    }

    #[test]
    fn tracker_keeps_block_comment_continuations_after_open_marker() {
        let mut tracker = BlockCommentTracker::new();
        assert!(tracker.chunk_is_comment_only("/* Summary.", CommentSyntax::CStyle));
        assert!(
            tracker
                .chunk_is_comment_only(" * Returns the value.\n *\n *****/", CommentSyntax::CStyle)
        );
        assert!(!tracker.chunk_is_comment_only("    * quantity;", CommentSyntax::CStyle));
    }

    #[test]
    fn chunk_is_comment_only_treats_hash_lines_as_code_in_c_style_languages() {
        assert!(!chunk_is_comment_only(
            "#define LIMIT 4\n#if DEBUG",
            CommentSyntax::CStyle
        ));
        assert!(chunk_is_comment_only(
            "# shell comment",
            CommentSyntax::CStyleOrHash
        ));
    }

    #[test]
    fn string_literal_slash_star_does_not_open_block_comment() {
        let mut tracker = BlockCommentTracker::new();
        assert!(!tracker.chunk_is_comment_only("let glob = \"/*\";", CommentSyntax::CStyle));
        assert!(!tracker.chunk_is_comment_only("    * scale;", CommentSyntax::CStyle));
        assert_eq!(
            leading_c_style_comment_prefix_len("/* doc\n * cont\n */\n    * scale;\n"),
            Some("/* doc\n * cont\n */\n".len())
        );
    }

    #[test]
    fn wrapped_multiplication_after_blank_line_is_code_not_comment() {
        assert!(
            !chunk_is_comment_only("    * quantity;", CommentSyntax::CStyle),
            "wrapped `* quantity;` without an open `/*` block comment must be code"
        );
        assert!(
            !chunk_is_comment_only("    * scale + offset;", CommentSyntax::CStyleOrHash),
            "wrapped `* scale + offset;` without an open `/*` block comment must be code"
        );
    }
}
