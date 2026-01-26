use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum Language {
    Rust,
    Elisp,
    JavaScript,
    TypeScript,
    Python,
    Shell,
    Markdown,
    Toml,
    Nix,
    Just,
    Text,
    #[default]
    Unknown,
}

// TODO: add Language::Go, Language::Java, Language::Cpp once tree-sitter support is wired.

impl Language {
    pub fn uses_text_fallback(&self) -> bool {
        matches!(
            self,
            Language::Text | Language::Toml | Language::Nix | Language::Just
        )
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Language::Rust),
            "el" => Some(Language::Elisp),
            "js" => Some(Language::JavaScript),
            "ts" => Some(Language::TypeScript),
            "py" => Some(Language::Python),
            "sh" => Some(Language::Shell),
            "md" | "markdown" => Some(Language::Markdown),
            "toml" => Some(Language::Toml),
            "nix" => Some(Language::Nix),
            "just" => Some(Language::Just),
            "org" | "txt" => Some(Language::Text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeFile {
    pub language: Language,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileType {
    Code(CodeFile),
    Markup,
    Binary,
    Text,
}

pub fn analyze_file(path: &Path) -> FileType {
    // 1. Check for extension-based Code/Markup
    if let Some(ext) = path.extension().and_then(|s| s.to_str())
        && let Some(language) = Language::from_extension(ext)
    {
        return FileType::Code(CodeFile { language });
    }

    // 2. Check for Binary (Heuristic: Read first 8kb, look for NULL)
    // We only want to read a small chunk, not the whole file if it's huge.
    // However, in `scanner.rs` we read the whole file anyway to hash it.
    // So we can pass the content if available, but `scanner.rs` calls us before chunking.
    // Let's just read the header here.

    if let Ok(mut file) = std::fs::File::open(path) {
        use std::io::Read;
        let mut buffer = [0; 1024]; // 1KB check is usually enough
        if let Ok(n) = file.read(&mut buffer) {
            let slice = &buffer[..n];
            if slice.contains(&0) {
                return FileType::Binary;
            }
        }
    }

    // Default to Text
    FileType::Text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_from_extension() {
        // Covers all match arms to ensure no dead code mutants
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("el"), Some(Language::Elisp));
        assert_eq!(Language::from_extension("js"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("sh"), Some(Language::Shell));
        assert_eq!(Language::from_extension("md"), Some(Language::Markdown));
        assert_eq!(
            Language::from_extension("markdown"),
            Some(Language::Markdown)
        );
        assert_eq!(Language::from_extension("toml"), Some(Language::Toml));
        assert_eq!(Language::from_extension("nix"), Some(Language::Nix));
        assert_eq!(Language::from_extension("just"), Some(Language::Just));
        assert_eq!(Language::from_extension("org"), Some(Language::Text));
        assert_eq!(Language::from_extension("txt"), Some(Language::Text));
        assert_eq!(Language::from_extension("unknown_ext"), None);
    }
}
