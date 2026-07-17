use std::path::Path;

use sha2::{Digest, Sha256};

use crate::analysis::Language;

use super::{
    DeclarationId, DeclarationKey, DeclarationKind, DeclarationProjectionHash, SourceComponent,
};

const PROJECTION_DOMAIN: &[u8] = b"trueflow.declaration.projection.v1";
const SNAPSHOT_ID_DOMAIN: &[u8] = b"trueflow.declaration.snapshot-id.v1";
const KEY_DOMAIN: &[u8] = b"trueflow.declaration.key.v1";

pub fn projection_hash(
    language: Language,
    kind: DeclarationKind,
    components: &[SourceComponent],
) -> DeclarationProjectionHash {
    let mut hasher = Sha256::new();
    write_frame(&mut hasher, PROJECTION_DOMAIN);
    write_frame(&mut hasher, language_tag(language).as_bytes());
    write_frame(&mut hasher, kind.protocol_tag().as_bytes());
    for component in components {
        write_frame(&mut hasher, component.role.protocol_tag().as_bytes());
        write_frame(&mut hasher, component.text.as_bytes());
    }
    DeclarationProjectionHash::new(format!("{:x}", hasher.finalize()))
}

pub(crate) fn declaration_id(
    path: &Path,
    kind: DeclarationKind,
    name: &str,
    source_ordinal: usize,
    start_byte: usize,
    projection_hash: &DeclarationProjectionHash,
) -> DeclarationId {
    let mut hasher = Sha256::new();
    write_frame(&mut hasher, SNAPSHOT_ID_DOMAIN);
    write_frame(&mut hasher, path.as_os_str().as_encoded_bytes());
    write_frame(&mut hasher, kind.protocol_tag().as_bytes());
    write_frame(&mut hasher, name.as_bytes());
    write_u64(&mut hasher, source_ordinal as u64);
    write_u64(&mut hasher, start_byte as u64);
    write_frame(&mut hasher, projection_hash.as_str().as_bytes());
    DeclarationId::new(format!("{:x}", hasher.finalize()))
}

pub(crate) fn declaration_key(
    language: Language,
    kind: DeclarationKind,
    name: &str,
    parent_name: Option<&str>,
    overload_discriminator: &str,
) -> DeclarationKey {
    let mut hasher = Sha256::new();
    write_frame(&mut hasher, KEY_DOMAIN);
    write_frame(&mut hasher, language_tag(language).as_bytes());
    write_frame(&mut hasher, kind.protocol_tag().as_bytes());
    write_frame(&mut hasher, parent_name.unwrap_or_default().as_bytes());
    write_frame(&mut hasher, name.as_bytes());
    write_frame(&mut hasher, overload_discriminator.as_bytes());
    DeclarationKey::new(format!("{:x}", hasher.finalize()))
}

fn write_frame(hasher: &mut Sha256, bytes: &[u8]) {
    write_u64(hasher, bytes.len() as u64);
    hasher.update(bytes);
}

fn write_u64(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_be_bytes());
}

fn language_tag(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::Swift => "swift",
        Language::Elisp => "elisp",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Java => "java",
        Language::Kotlin => "kotlin",
        Language::CSharp => "csharp",
        Language::Python => "python",
        Language::Ruby => "ruby",
        Language::Php => "php",
        Language::Go => "go",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::Zig => "zig",
        Language::Lua => "lua",
        Language::Dart => "dart",
        Language::Scala => "scala",
        Language::Haskell => "haskell",
        Language::OCaml => "ocaml",
        Language::Elixir => "elixir",
        Language::Clojure => "clojure",
        Language::Sql => "sql",
        Language::Yaml => "yaml",
        Language::Json => "json",
        Language::Html => "html",
        Language::Css => "css",
        Language::Shell => "shell",
        Language::Markdown => "markdown",
        Language::Toml => "toml",
        Language::Nix => "nix",
        Language::Just => "just",
        Language::Text => "text",
        Language::Unknown => "unknown",
    }
}
