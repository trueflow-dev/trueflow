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
    Text,
    #[default]
    Unknown,
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
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        match ext {
            "rs" => {
                return FileType::Code(CodeFile {
                    language: Language::Rust,
                });
            }
            "el" => {
                return FileType::Code(CodeFile {
                    language: Language::Elisp,
                });
            }
            "js" => {
                return FileType::Code(CodeFile {
                    language: Language::JavaScript,
                });
            }
            "ts" => {
                return FileType::Code(CodeFile {
                    language: Language::TypeScript,
                });
            }
            "py" => {
                return FileType::Code(CodeFile {
                    language: Language::Python,
                });
            }
            "sh" => {
                return FileType::Code(CodeFile {
                    language: Language::Shell,
                });
            }
            "md" | "markdown" => {
                return FileType::Code(CodeFile {
                    language: Language::Markdown,
                });
            }
            "org" | "txt" => {
                return FileType::Code(CodeFile {
                    language: Language::Text,
                })
            }
            _ => {} // Fallthrough to binary check
        }
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
