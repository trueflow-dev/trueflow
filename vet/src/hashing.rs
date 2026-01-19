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
    let content_hash = hash_content(body);
    let context_hash = hash_content(context);

    Fingerprint {
        content_hash,
        context_hash,
    }
}

fn hash_content(input: &str) -> String {
    let mut hasher = Sha256::new();
    // No normalization for now, matching exact content.
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_no_normalization() {
        // Now whitespace is load-bearing
        let a = "  let x = 5; ";
        let b = "let x = 5;";
        let fp_a = compute_fingerprint(a, "");
        let fp_b = compute_fingerprint(b, "");
        assert_ne!(fp_a.content_hash, fp_b.content_hash);
    }
}
