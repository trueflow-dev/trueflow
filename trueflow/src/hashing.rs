use sha2::{Digest, Sha256};

pub struct Fingerprint {
    pub content_hash: String,
    pub context_hash: String,
}

impl Fingerprint {
    pub fn as_string(&self) -> String {
        // We combine them to form the final fingerprint string
        let mut hasher = Sha256::new();
        hasher.update(&self.content_hash);
        hasher.update(&self.context_hash);
        format!("{:x}", hasher.finalize())
    }
}

pub fn compute_fingerprint(body: &str, context: &str) -> Fingerprint {
    let content_hash = hash_str(body);
    let context_hash = hash_str(context);

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

    // Normalize line endings and trim trailing whitespace per line
    for line in input.lines() {
        let trimmed = line.trim_end();
        output.push_str(trimmed);
        output.push('\n');
    }

    // Ensure empty input remains empty?
    if input.is_empty() {
        return String::new();
    }

    // If the input was just whitespace, lines() might be empty or not iterate what we expect?
    // "   " -> lines() -> ["   "] -> trim -> "" -> push \n -> "\n"
    // "" -> lines() -> [] -> ""
    // "\n" -> lines() -> [""] -> "" -> push \n -> "\n"

    // Actually, lines() handles \r\n and \n.
    // If input ends with newline, lines() does NOT yield a final empty string.
    // If input is "a\n", lines is "a". Output "a\n".
    // If input is "a", lines is "a". Output "a\n".
    // So this enforces a trailing newline.

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stability_snapshot() {
        // Regression test: Ensures the hashing algorithm doesn't drift.
        // If this test fails, it means fingerprints have changed and existing
        // review records may no longer match their blocks.
        let body = "fn main() {\n    println!(\"hello\");\n}";
        let context = "use std::io;";

        let fp = compute_fingerprint(body, context);

        // This hash was computed with the current canonicalization logic.
        // DO NOT change this value unless intentionally changing the hashing algorithm.
        assert_eq!(
            fp.as_string(),
            "dc1c606ceaac3fe3f3e6c11d170d950e290cbf509cf87b905c08b0f0503178c7",
            "Fingerprint hash changed! This will break existing review records."
        );
    }

    #[test]
    fn test_context_separation() {
        // Ensure Body="AB", Context="" != Body="A", Context="B"
        let fp1 = compute_fingerprint("AB", "");
        let fp2 = compute_fingerprint("A", "B");
        assert_ne!(fp1.as_string(), fp2.as_string());
    }

    #[test]
    fn test_hash_str_snapshot() {
        // 'hello' -> 'hello\n' via canonicalize
        // So hash will change from raw 'hello'.
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
        ); // Wait, line.trim_end() does this

        // However, internal newlines matter?
        assert_ne!(hash_str("a\nb"), hash_str("ab"));
    }

    #[test]
    fn test_canonicalize_logic() {
        assert_eq!(canonicalize("foo"), "foo\n");
        assert_eq!(canonicalize("foo\n"), "foo\n");
        assert_eq!(canonicalize("foo\r\n"), "foo\n");
        assert_eq!(canonicalize("foo  \n"), "foo\n");
        assert_eq!(canonicalize("  foo"), "  foo\n"); // Leading whitespace preserved
        assert_eq!(canonicalize(""), "");
    }

    #[test]
    fn test_fingerprint_components() {
        let body = "fn main() {}\n";
        let context = "use std::fmt;";
        let fp = compute_fingerprint(body, context);

        assert_eq!(fp.content_hash, hash_str(body));
        assert_eq!(fp.context_hash, hash_str(context));
    }
}
