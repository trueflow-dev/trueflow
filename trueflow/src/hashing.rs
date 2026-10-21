use anyhow::{Result, anyhow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
pub(crate) fn hex_digest<D: AsRef<[u8]>>(digest: D) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let bytes = digest.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

macro_rules! define_hash_type {
    ($name:ident) => {
        #[derive(
            Serialize,
            Deserialize,
            Debug,
            Clone,
            PartialEq,
            Eq,
            Hash,
            Default,
            PartialOrd,
            Ord,
            JsonSchema,
        )]
        #[serde(transparent)]
        #[schemars(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }
    };
}

define_hash_type!(BytesHash);
define_hash_type!(TreeHash);

impl BytesHash {
    pub fn from_bytes(input: &[u8]) -> Self {
        Self::new(hash_bytes(input))
    }
}

fn parse_hash_value(kind: &str, value: impl AsRef<str>) -> Result<String> {
    let value = value.as_ref().trim();
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(anyhow!("{kind} must be a 64-character hex string: {value}"));
    }
    Ok(value.to_ascii_lowercase())
}

impl TreeHash {
    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        Ok(Self::new(parse_hash_value("TreeHash", value)?))
    }

    pub fn from_content(input: &str) -> Self {
        Self::new(hash_str(input))
    }

    pub fn from_bytes_hash(bytes_hash: &BytesHash) -> Self {
        Self::new(bytes_hash.as_str())
    }

    pub fn from_child_hashes<'a, I>(hashes: I) -> Self
    where
        I: IntoIterator<Item = &'a TreeHash>,
    {
        let mut hasher = Sha256::new();
        for hash in hashes {
            hasher.update(hash.as_str());
        }
        Self::new(hex_digest(hasher.finalize()))
    }
}

pub fn hash_str(input: &str) -> String {
    let mut hasher = Sha256::new();
    emit_canonical(input, |piece| hasher.update(piece));
    hex_digest(hasher.finalize())
}

pub fn hash_bytes(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hex_digest(hasher.finalize())
}

/// Normalize content for hashing.
/// - Trims trailing whitespace from lines.
/// - Replaces Windows/Mac line endings with \n.
/// - Ensures a single trailing newline.
#[cfg(test)]
fn canonicalize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    emit_canonical(input, |piece| output.push_str(piece));
    output
}

/// Emits the canonical form of `input` (see `canonicalize`) as consecutive
/// pieces. Runs of lines that canonicalization leaves unchanged are emitted
/// as one borrowed slice, so hashing a block needs no copy of its content.
fn emit_canonical<'a>(input: &'a str, mut emit: impl FnMut(&'a str)) {
    let bytes = input.as_bytes();
    let mut run_start = 0;
    let mut line_start = 0;
    let mut index = 0;

    while index < bytes.len() {
        let terminator = bytes[index];
        if matches!(terminator, b'\r' | b'\n') {
            let line = &input[line_start..index];
            let kept_len = line.trim_end().len();
            if terminator == b'\r' || kept_len != line.len() {
                emit(&input[run_start..line_start + kept_len]);
                emit("\n");
                if terminator == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                run_start = index + 1;
            }
            line_start = index + 1;
        }
        index += 1;
    }

    if line_start < input.len() {
        emit(&input[run_start..line_start + input[line_start..].trim_end().len()]);
        emit("\n");
    } else {
        emit(&input[run_start..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_text_hash_has_stable_protocol_fingerprint() {
        assert_eq!(
            hash_str("hello").as_str(),
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
    }

    /// The line-by-line algorithm that defined canonical text before hashing
    /// streamed borrowed runs. Block hashes persist in review records, so the
    /// streaming form must never diverge from it.
    fn canonicalize_reference(input: &str) -> String {
        let mut output = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut line_start = 0;
        let mut index = 0;
        while index < bytes.len() {
            if matches!(bytes[index], b'\r' | b'\n') {
                output.push_str(input[line_start..index].trim_end());
                output.push('\n');
                if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                line_start = index + 1;
            }
            index += 1;
        }
        if line_start < input.len() {
            output.push_str(input[line_start..].trim_end());
            output.push('\n');
        }
        output
    }

    #[test]
    fn streamed_canonical_hash_matches_reference_for_every_short_string() {
        const ALPHABET: [char; 6] = ['a', ' ', '\t', '\r', '\n', '\u{a0}'];
        let mut digits = Vec::new();
        loop {
            let input = digits
                .iter()
                .map(|&digit| ALPHABET[digit])
                .collect::<String>();
            let reference = canonicalize_reference(&input);
            assert_eq!(canonicalize(&input), reference, "{input:?}");
            assert_eq!(
                hash_str(&input),
                hex_digest(Sha256::digest(reference.as_bytes())),
                "{input:?}"
            );

            // Odometer over every string of up to six alphabet characters.
            let Some(position) = digits.iter().rposition(|&digit| digit + 1 < ALPHABET.len())
            else {
                if digits.len() == 6 {
                    break;
                }
                digits = vec![0; digits.len() + 1];
                continue;
            };
            digits[position] += 1;
            for digit in &mut digits[position + 1..] {
                *digit = 0;
            }
        }
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
            hash_str("first\rsecond"),
            hash_str("first\nsecond"),
            "Lone CR line endings should be normalized"
        );
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
        assert_eq!(canonicalize("foo\rbar\r"), "foo\nbar\n");
        assert_eq!(canonicalize("foo  \n"), "foo\n");
        assert_eq!(canonicalize("  foo"), "  foo\n");
        assert_eq!(canonicalize(""), "");
        // Changed lines between unchanged runs must splice back exactly.
        assert_eq!(canonicalize("a\nb \nc\n"), "a\nb\nc\n");
        assert_eq!(canonicalize("a\r\nb\nc"), "a\nb\nc\n");
        assert_eq!(canonicalize("a\n  \n"), "a\n\n");
        assert_eq!(canonicalize("\n\n"), "\n\n");
        assert_eq!(canonicalize("x\u{a0}\ny\t"), "x\ny\n");
        assert_eq!(hash_str("a\nb \nc"), hash_str("a\nb\nc\n"));
    }

    #[test]
    fn test_bytes_hash_serializes_as_string() -> Result<(), serde_json::Error> {
        let hash = BytesHash::new("abc123");
        let json = serde_json::to_string(&hash)?;
        let round_trip: BytesHash = serde_json::from_str(&json)?;

        assert_eq!(json, "\"abc123\"");
        assert_eq!(round_trip, hash);
        assert_eq!(round_trip.as_str(), "abc123");
        Ok(())
    }

    #[test]
    fn test_bytes_hash_is_exact_over_raw_bytes() {
        assert_ne!(
            BytesHash::from_bytes(b"line\n"),
            BytesHash::from_bytes(b"line\r\n")
        );
        assert_ne!(
            BytesHash::from_bytes(b"line"),
            BytesHash::from_bytes(b"line\n")
        );
    }

    #[test]
    fn test_tree_hash_can_reuse_bytes_hash_for_leaf_files() {
        let bytes_hash = BytesHash::from_bytes(b"raw-bytes");
        let tree_hash = TreeHash::from_bytes_hash(&bytes_hash);

        assert_eq!(tree_hash.as_str(), bytes_hash.as_str());
    }
}
