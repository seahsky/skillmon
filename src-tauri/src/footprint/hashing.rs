use sha2::{Digest, Sha256};

pub fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_hashes_identically() {
        assert_eq!(sha256_hex("Base directory for this skill: /a\n\nBody."), sha256_hex("Base directory for this skill: /a\n\nBody."));
    }

    #[test]
    fn different_content_hashes_differently() {
        assert_ne!(sha256_hex("Body one."), sha256_hex("Body two."));
    }

    #[test]
    fn output_is_a_lowercase_64_char_hex_string() {
        let hash = sha256_hex("some skill text");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
