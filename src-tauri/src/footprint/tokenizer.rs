use tiktoken_rs::o200k_base_singleton;

/// The calibrated-default tier (ADR 0006): `o200k_base`, never surfaced to
/// the user as a model choice -- just the honest-estimate baseline that gets
/// scaled by a calibration factor once an exact sample exists.
pub fn estimate_tokens(text: &str) -> u32 {
    o200k_base_singleton().encode_ordinary(text).len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_zero_tokens() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn longer_text_has_more_tokens_than_a_strict_substring() {
        let body = "Base directory for this skill: /a/b/c\n\nInterview the user relentlessly about every aspect of this plan.";
        let prefix = "Base directory for this skill: /a/b/c";

        assert!(estimate_tokens(body) > estimate_tokens(prefix));
    }

    #[test]
    fn identical_input_is_deterministic() {
        let text = "Interview the user relentlessly about every aspect of this plan.";
        assert_eq!(estimate_tokens(text), estimate_tokens(text));
    }
}
