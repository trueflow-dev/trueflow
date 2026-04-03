use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, Default, JsonSchema)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn from_content(input: &str) -> Self {
        Self::new(hash_str(input))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for ContentHash {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<String> for ContentHash {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for ContentHash {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

pub struct Fingerprint {
    pub content_hash: ContentHash,
    pub context_hash: ContentHash,
}

impl Fingerprint {
    pub fn as_string(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.content_hash.as_str());
        hasher.update(self.context_hash.as_str());
        format!("{:x}", hasher.finalize())
    }
}

pub fn compute_fingerprint(body: &str, context: &str) -> Fingerprint {
    let content_hash = ContentHash::from_content(body);
    let context_hash = ContentHash::from_content(context);

    Fingerprint {
        content_hash,
        context_hash,
    }
}

pub fn hash_str(input: &str) -> String {
    let mut hasher = Sha256::new();
    let normalized = canonicalize(input);
    hasher.update(normalized);
    format!("{:x}", hasher.finalize())
}

/// Normalize content for hashing.
/// - Trims trailing whitespace from lines.
/// - Replaces Windows/Mac line endings with \n.
/// - Ensures a single trailing newline.
pub fn canonicalize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());

    for line in input.lines() {
        let trimmed = line.trim_end();
        output.push_str(trimmed);
        output.push('\n');
    }

    if input.is_empty() {
        return String::new();
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stability_snapshot() {
        let body = "fn main() {\n    println!(\"hello\");\n}";
        let context = "use std::io;";

        let fp = compute_fingerprint(body, context);

        assert_eq!(
            fp.as_string(),
            "dc1c606ceaac3fe3f3e6c11d170d950e290cbf509cf87b905c08b0f0503178c7",
            "Fingerprint hash changed! This will break existing review records."
        );
    }

    #[test]
    fn test_context_separation() {
        let fp1 = compute_fingerprint("AB", "");
        let fp2 = compute_fingerprint("A", "B");
        assert_ne!(fp1.as_string(), fp2.as_string());
    }

    #[test]
    fn test_hash_str_snapshot() {
        let raw_hello_hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_ne!(hash_str("hello"), raw_hello_hash);
    }

    #[test]
    fn test_hash_str_is_whitespace_insensitive_for_formatting() {
        let base = hash_str("line");
        assert_eq!(
            base,
            hash_str("line\n"),
            "Trailing newline should be normalized"
        );
        assert_eq!(base, hash_str("line\r\n"), "CRLF should be normalized");
        assert_eq!(
            base,
            hash_str("line  "),
            "Trailing spaces on line should be trimmed"
        );
        assert_ne!(hash_str("a\nb"), hash_str("ab"));
    }

    #[test]
    fn test_canonicalize_logic() {
        assert_eq!(canonicalize("foo"), "foo\n");
        assert_eq!(canonicalize("foo\n"), "foo\n");
        assert_eq!(canonicalize("foo\r\n"), "foo\n");
        assert_eq!(canonicalize("foo  \n"), "foo\n");
        assert_eq!(canonicalize("  foo"), "  foo\n");
        assert_eq!(canonicalize(""), "");
    }

    #[test]
    fn test_fingerprint_components() {
        let body = "fn main() {}\n";
        let context = "use std::fmt;";
        let fp = compute_fingerprint(body, context);

        assert_eq!(fp.content_hash, ContentHash::from_content(body));
        assert_eq!(fp.context_hash, ContentHash::from_content(context));
    }

    #[test]
    fn test_content_hash_serializes_as_string() -> Result<(), serde_json::Error> {
        let hash = ContentHash::new("abc123");
        let json = serde_json::to_string(&hash)?;
        let round_trip: ContentHash = serde_json::from_str(&json)?;

        assert_eq!(json, "\"abc123\"");
        assert_eq!(round_trip, hash);
        assert_eq!(round_trip.as_str(), "abc123");
        Ok(())
    }
}
