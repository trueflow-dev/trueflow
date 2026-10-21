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

pub fn line_is_c_style_comment(trimmed_line: &str) -> bool {
    trimmed_line.starts_with("//")
        || trimmed_line.starts_with("/*")
        || is_block_comment_continuation(trimmed_line)
}

/// A `*` line inside a block comment (` * text`, `*/`, a `****` banner), as
/// opposed to code such as a dereference (`*out = v;`, `**pp = v;`).
fn is_block_comment_continuation(trimmed_line: &str) -> bool {
    let Some(rest) = trimmed_line.strip_prefix('*') else {
        return false;
    };
    let rest = rest.trim_start_matches('*');
    rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('/')
}

pub fn chunk_is_comment_only(chunk: &str, syntax: CommentSyntax) -> bool {
    let mut in_block_comment = false;
    let mut saw_comment = false;

    for source_line in chunk.lines() {
        let mut line = source_line.trim_start();
        loop {
            if line.is_empty() {
                break;
            }

            if in_block_comment {
                saw_comment = true;
                if let Some(end) = line.find("*/") {
                    line = line[end + 2..].trim_start();
                    in_block_comment = false;
                    continue;
                }
                break;
            }

            if syntax.starts_line_comment(line) {
                saw_comment = true;
                break;
            }

            if let Some(comment) = line.strip_prefix("/*") {
                saw_comment = true;
                in_block_comment = true;
                line = comment;
                continue;
            }

            // Split chunks can begin inside a block comment, before a leading `*`.
            if is_block_comment_continuation(line) {
                saw_comment = true;
                if let Some(end) = line.find("*/") {
                    line = line[end + 2..].trim_start();
                    continue;
                }
                break;
            }

            return false;
        }
    }

    saw_comment
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
        assert!(!line_is_c_style_comment("*out = value;"));
    }

    #[test]
    fn chunk_is_comment_only_keeps_block_comment_continuations() {
        assert!(chunk_is_comment_only(
            " * Returns the value.\n *\n *****/",
            CommentSyntax::CStyle
        ));
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
    fn line_is_c_style_comment_accepts_closing_block_markers() {
        assert!(line_is_c_style_comment("*/"));
    }
}
