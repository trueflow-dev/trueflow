use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Swift,
    Elisp,
    JavaScript,
    TypeScript,
    Java,
    Kotlin,
    CSharp,
    Python,
    Ruby,
    Go,
    Cpp,
    Shell,
    Markdown,
    Toml,
    Nix,
    Just,
    Text,
    #[default]
    Unknown,
}

impl Language {
    pub fn uses_text_fallback(self) -> bool {
        matches!(self, Language::Text | Language::Toml)
    }

    pub fn from_file_name(file_name: &str) -> Option<Self> {
        match file_name {
            name if name.eq_ignore_ascii_case("justfile") => Some(Language::Just),
            _ => None,
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Language::Rust),
            "swift" => Some(Language::Swift),
            "el" => Some(Language::Elisp),
            "js" => Some(Language::JavaScript),
            "ts" => Some(Language::TypeScript),
            "java" => Some(Language::Java),
            "kt" | "kts" => Some(Language::Kotlin),
            "cs" => Some(Language::CSharp),
            "py" => Some(Language::Python),
            "rb" => Some(Language::Ruby),
            "go" => Some(Language::Go),
            "cpp" | "cxx" | "cc" | "hpp" | "hh" | "hxx" | "h++" => Some(Language::Cpp),
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

    // 2. Check for canonical filename-based Code detection.
    if let Some(file_name) = path.file_name().and_then(|name| name.to_str())
        && let Some(language) = Language::from_file_name(file_name)
    {
        return FileType::Code(CodeFile { language });
    }

    // 3. Check for Binary (Heuristic: Read first 8kb, look for NULL)
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
            if let Some(language) = language_from_shebang(slice) {
                return FileType::Code(CodeFile { language });
            }
        }
    }

    // Default to Text
    FileType::Text
}

fn language_from_shebang(header: &[u8]) -> Option<Language> {
    let line = std::str::from_utf8(header)
        .ok()?
        .lines()
        .next()?
        .strip_prefix("#!")?
        .trim();
    let mut tokens = line.split_whitespace();
    let command = tokens.next()?;
    let interpreter = if command.ends_with("/env") {
        tokens
            .find(|token| !token.starts_with('-'))
            .map(|token| token.rsplit('/').next().unwrap_or(token))?
    } else {
        command.rsplit('/').next()?
    };

    match interpreter {
        "sh" | "bash" | "zsh" | "dash" | "ksh" => Some(Language::Shell),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempPath {
        path: PathBuf,
    }

    impl TempPath {
        fn new(path: PathBuf) -> Self {
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let mut current = self.path.parent();
            while let Some(dir) = current {
                if !dir.starts_with(std::env::temp_dir()) {
                    break;
                }
                if fs::remove_dir(dir).is_err() {
                    break;
                }
                current = dir.parent();
            }
        }
    }

    fn write_temp_file(name: &str, contents: &[u8]) -> TempPath {
        static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let unique_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "trueflow-analysis-tests-{}-{timestamp}-{unique_id}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create temp directory: {error}"));
        let path = dir.join(name);
        fs::write(&path, contents).unwrap_or_else(|error| panic!("write temp file: {error}"));
        TempPath::new(path)
    }

    #[test]
    fn test_language_from_extension() {
        // Covers all match arms to ensure no dead code mutants
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("swift"), Some(Language::Swift));
        assert_eq!(Language::from_extension("el"), Some(Language::Elisp));
        assert_eq!(Language::from_extension("js"), Some(Language::JavaScript));
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("java"), Some(Language::Java));
        assert_eq!(Language::from_extension("kt"), Some(Language::Kotlin));
        assert_eq!(Language::from_extension("kts"), Some(Language::Kotlin));
        assert_eq!(Language::from_extension("cs"), Some(Language::CSharp));
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("rb"), Some(Language::Ruby));
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        assert_eq!(Language::from_extension("cpp"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("cxx"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("cc"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("hpp"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("hh"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("hxx"), Some(Language::Cpp));
        assert_eq!(Language::from_extension("h++"), Some(Language::Cpp));
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

    #[test]
    fn test_language_from_file_name() {
        assert_eq!(Language::from_file_name("Justfile"), Some(Language::Just));
        assert_eq!(Language::from_file_name("justfile"), Some(Language::Just));
        assert_eq!(Language::from_file_name("main.rs"), None);
    }

    #[test]
    fn analyze_file_detects_justfile_without_extension() {
        let file = write_temp_file("Justfile", b"default:\n    echo hello\n");

        assert!(matches!(
            analyze_file(file.path()),
            FileType::Code(CodeFile {
                language: Language::Just
            })
        ));
    }

    #[test]
    fn analyze_file_detects_shell_shebang_without_extension() {
        let file = write_temp_file("script", b"#!/usr/bin/env bash\necho hello\n");

        assert!(matches!(
            analyze_file(file.path()),
            FileType::Code(CodeFile {
                language: Language::Shell
            })
        ));
    }

    #[test]
    fn analyze_file_detects_java_by_extension() {
        let file = write_temp_file("Main.java", b"class Main {}\n");

        assert!(matches!(
            analyze_file(file.path()),
            FileType::Code(CodeFile {
                language: Language::Java
            })
        ));
    }

    #[test]
    fn analyze_file_detects_kotlin_by_extension() {
        let file = write_temp_file("Main.kt", b"class Main\n");

        assert!(matches!(
            analyze_file(file.path()),
            FileType::Code(CodeFile {
                language: Language::Kotlin
            })
        ));
    }

    #[test]
    fn analyze_file_detects_kotlin_script_by_extension() {
        let file = write_temp_file("build.main.kts", b"val answer = 42\n");

        assert!(matches!(
            analyze_file(file.path()),
            FileType::Code(CodeFile {
                language: Language::Kotlin
            })
        ));
    }

    #[test]
    fn analyze_file_detects_csharp_by_extension() {
        let file = write_temp_file("Program.cs", b"class Program {}\n");

        assert!(matches!(
            analyze_file(file.path()),
            FileType::Code(CodeFile {
                language: Language::CSharp
            })
        ));
    }

    #[test]
    fn analyze_file_detects_ruby_by_extension() {
        let file = write_temp_file("app.rb", b"class App\nend\n");

        assert!(matches!(
            analyze_file(file.path()),
            FileType::Code(CodeFile {
                language: Language::Ruby
            })
        ));
    }
}
