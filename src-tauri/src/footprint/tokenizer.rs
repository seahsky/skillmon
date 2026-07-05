/// The calibrated-default tier (ADR 0006): `o200k_base`, never surfaced to
/// the user as a model choice -- just the honest-estimate baseline that gets
/// scaled by a calibration factor once an exact sample exists.
///
/// Backed by `bpe-openai`'s linear-time o200k_base tokenizer rather than
/// `tiktoken-rs`'s fancy-regex one. The two are byte-for-byte identical on
/// this encoding (proven by `parity_with_tiktoken_over_a_corpus_and_edge_cases`
/// below), so the swap changes no displayed count -- it only removes the slow
/// fancy-regex backtracking that dominated the first-scan cost (ADR 0006 update).
pub fn estimate_tokens(text: &str) -> u32 {
    bpe_openai::o200k_base().count(text) as u32
}

/// The calibrated-estimate tokenizer as an injectable seam. Production wires
/// `BpeTokenizer` (a thin call through to `estimate_tokens`); a test can
/// substitute a spy to prove which texts a scan actually tokenizes -- the
/// negative proof behind issue #11's "interactive scan does zero on-demand
/// tokenization." `Send + Sync` so the adapter can hold it as `Box<dyn
/// Tokenizer>` behind the scan `Mutex` and a background pass can hold its own.
pub trait Tokenizer: Send + Sync {
    fn estimate(&self, text: &str) -> u32;
}

/// The one production `Tokenizer`: identical output to the free
/// `estimate_tokens`, so wiring it changes no displayed count.
pub struct BpeTokenizer;

impl Tokenizer for BpeTokenizer {
    fn estimate(&self, text: &str) -> u32 {
        estimate_tokens(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_zero_tokens() {
        assert_eq!(estimate_tokens(""), 0);
    }

    /// The injectable `BpeTokenizer` must be byte-identical to the free
    /// function it wraps, or wiring the seam (issue #11) would silently shift
    /// every estimate-tier count.
    #[test]
    fn bpe_tokenizer_estimate_matches_the_free_function() {
        let tok = BpeTokenizer;
        for text in edge_case_corpus() {
            assert_eq!(tok.estimate(&text), estimate_tokens(&text), "drift on {text:?}");
        }
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

    /// Strings chosen to stress exactly where a linear-regex BPE could drift
    /// from tiktoken's fancy-regex o200k pretokenization: digit runs,
    /// contractions/apostrophes, whitespace and trailing-space-before-newline,
    /// CJK, emoji, base64, and literal special-token spellings (which
    /// `encode_ordinary` must treat as ordinary text, never as control tokens).
    fn edge_case_corpus() -> Vec<String> {
        vec![
            String::new(),
            " ".to_string(),
            "1234567".to_string(),
            "1,234,567.89".to_string(),
            "it's don't 'twas y'all o'clock".to_string(),
            "line one\n\n   \t  trailing\t tabs and spaces   \nnext".to_string(),
            "trailing spaces before newline   \nmore".to_string(),
            "这是一个用于监控技能上下文足迹的测试句子。".to_string(),
            "混合 mixed 语言 text with 数字 12345 and码 code".to_string(),
            "🚀🔥✨👍🏽🧑‍💻🇸🇬".to_string(),
            "aGVsbG8gd29ybGQgdGhpcyBpcyBhIGJhc2U2NCBibG9i".to_string(),
            "<|endoftext|> <|im_start|>system<|im_sep|>".to_string(),
            "---\nname: grilling\ndescription: Interview the user relentlessly.\n---\n".to_string(),
            "```rust\nfn main() {\n    let x = 42;\n    println!(\"{x}\");\n}\n```".to_string(),
            // Punctuation/slash runs exercise o200k's ` ?[^\s\p{L}\p{N}]+[\r\n/]*`
            // pretokenization branch, the most likely place a re-implemented
            // regex could diverge from tiktoken's.
            "https://example.com//a///b/c?x=1&y=2#frag".to_string(),
            "/usr/local/bin//foo///bar\\\\baz".to_string(),
            "!!!???...,,,;;;:::///|||===>>><<<".to_string(),
            "Base directory for this skill: /Users/x/.claude/skills/foo\n\nDo the thing."
                .to_string(),
        ]
    }

    /// The real drift guard (the existing footprint tests are tautological --
    /// both sides call `estimate_tokens`). `bpe-openai`'s o200k_base must be
    /// byte-for-byte identical to `tiktoken-rs`'s o200k_base, so the no-key
    /// headline -- the raw estimate for the majority of users, with no
    /// calibration factor to absorb any divergence -- is provably unchanged by
    /// the swap. Exact equality, never a tolerance (ADR 0006 update). This
    /// embedded corpus is the always-on guard; the real-skill-body sweep and
    /// the release benchmark below deepen it on a developer machine, and
    /// bpe-openai fuzzes full-sequence equivalence to tiktoken upstream.
    #[test]
    fn parity_with_tiktoken_over_a_corpus_and_edge_cases() {
        use tiktoken_rs::o200k_base_singleton;
        let oracle = o200k_base_singleton();

        for text in edge_case_corpus() {
            let bpe = estimate_tokens(&text);
            let tiktoken = oracle.encode_ordinary(&text).len() as u32;
            assert_eq!(bpe, tiktoken, "count drift on {text:?}: bpe={bpe} tiktoken={tiktoken}");
        }
    }

    /// Sweeps this machine's real personal skill bodies when present, so the
    /// parity guarantee is checked against genuine SKILL.md content, not just
    /// hand-picked edges. Silently no-ops in a fixtureless CI environment;
    /// the embedded corpus above is the always-on guard.
    #[test]
    fn parity_with_tiktoken_over_real_skill_bodies_when_available() {
        use tiktoken_rs::o200k_base_singleton;
        let Some(home) = dirs::home_dir() else { return };
        let skills_dir = home.join(".claude").join("skills");
        let Ok(entries) = std::fs::read_dir(&skills_dir) else { return };
        let oracle = o200k_base_singleton();

        let mut checked = 0usize;
        for entry in entries.flatten() {
            let skill_md = entry.path().join("SKILL.md");
            let Ok(body) = std::fs::read_to_string(&skill_md) else { continue };
            let bpe = estimate_tokens(&body);
            let tiktoken = oracle.encode_ordinary(&body).len() as u32;
            assert_eq!(
                bpe, tiktoken,
                "count drift on {}: bpe={bpe} tiktoken={tiktoken}",
                skill_md.display()
            );
            checked += 1;
        }
        eprintln!("parity confirmed on {checked} real skill bodies");
    }

    /// Reproducible, network-free before/after for ADR 0006's "measured"
    /// acceptance criterion: times `bpe-openai` vs `tiktoken-rs` over this
    /// machine's real skill corpus (SKILL.md bodies + every bundled file, i.e.
    /// the on-demand ceiling texts -- the bulk of the cold-scan token volume).
    /// Both tokenizers run in the same process so the comparison is apples to
    /// apples, and total counts must match (parity). Build in RELEASE or the
    /// numbers are a debug artifact:
    /// `cargo test --release --manifest-path src-tauri/Cargo.toml
    ///   footprint::tokenizer::tests::release_benchmark_bpe_vs_tiktoken -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn release_benchmark_bpe_vs_tiktoken() {
        use std::time::Instant;
        use tiktoken_rs::o200k_base_singleton;

        fn collect(dir: &std::path::Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, out);
                } else if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push(text);
                }
            }
        }

        let home = dirs::home_dir().expect("home dir");
        let mut corpus = Vec::new();
        collect(&home.join(".claude").join("skills"), &mut corpus);
        if corpus.is_empty() {
            eprintln!("no ~/.claude/skills corpus on this machine; skipping benchmark");
            return;
        }
        let bytes: usize = corpus.iter().map(|s| s.len()).sum();

        let oracle = o200k_base_singleton();
        let t0 = Instant::now();
        let tiktoken_total: u64 =
            corpus.iter().map(|s| oracle.encode_ordinary(s).len() as u64).sum();
        let tiktoken_elapsed = t0.elapsed();

        let t1 = Instant::now();
        let bpe_total: u64 = corpus.iter().map(|s| estimate_tokens(s) as u64).sum();
        let bpe_elapsed = t1.elapsed();

        eprintln!(
            "corpus: {} files, {:.1} MB, {} tokens\n  tiktoken-rs: {:?}\n  bpe-openai:  {:?}  ({:.2}x)",
            corpus.len(),
            bytes as f64 / 1e6,
            bpe_total,
            tiktoken_elapsed,
            bpe_elapsed,
            tiktoken_elapsed.as_secs_f64() / bpe_elapsed.as_secs_f64(),
        );
        assert_eq!(bpe_total, tiktoken_total, "total token counts must match across the whole corpus");
    }
}
