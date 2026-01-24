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
    // No normalization for now, matching exact content.
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stability_snapshot() {
        // Regression test: Ensures the hashing algorithm doesn't drift
        let body = "fn main() {\n    println!(\"hello\");\n}";
        let context = "use std::io;";

        let fp = compute_fingerprint(body, context);

        // Expected value derived from current implementation
        assert_eq!(
            fp.as_string(),
            "70b4fcde92d601906732332e0908eb304aa0b7e374d03f0dab65b2311c10a75d"
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
        assert_eq!(
            hash_str("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_hash_str_is_whitespace_sensitive() {
        let base = hash_str("line");
        assert_ne!(base, hash_str("line\n"));
        assert_ne!(base, hash_str("line\r\n"));
        assert_ne!(hash_str("line\n"), hash_str("line\r\n"));
    }

    #[test]
    fn test_fingerprint_components() {
        let body = "fn main() {}\n";
        let context = "use std::fmt;";
        let fp = compute_fingerprint(body, context);

        assert_eq!(fp.content_hash, hash_str(body));
        assert_eq!(fp.context_hash, hash_str(context));

        let combined = format!("{}{}", fp.content_hash, fp.context_hash);
        assert_eq!(fp.as_string(), hash_str(&combined));
    }
}
