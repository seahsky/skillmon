use crate::domain::footprint::{LayerCount, TokenSource};
use crate::footprint::api_key_store::ApiKeyStore;
use crate::footprint::cache::TokenCache;
use crate::footprint::count_tokens_client::CountTokensClient;
use crate::footprint::hashing::sha256_hex;
use crate::footprint::tokenizer::Tokenizer;

/// Counts `text` following ADR 0006 + ADR 0018's priority order: a fresh
/// exact value under the current reference model wins outright; otherwise a
/// live `count_tokens` call is attempted if a key is configured; a failed
/// call and "no key configured" share the same calibrated-estimate fallback,
/// since both mean "exact isn't available right now."
///
/// The estimate goes through the injected `tokenizer` rather than the free
/// `estimate_tokens` so a scan can prove which texts it actually tokenizes
/// (issue #11's spy seam); `BpeTokenizer` is the only production impl and is
/// byte-identical to the free function.
pub fn count_text(
    text: &str,
    cache: &TokenCache,
    api_key_store: &dyn ApiKeyStore,
    client: &dyn CountTokensClient,
    tokenizer: &dyn Tokenizer,
    reference_model_id: &str,
) -> LayerCount {
    let hash = sha256_hex(text);
    let cached = cache.get(&hash);

    if let Some(entry) = &cached {
        if let Some((exact_tokens, model_id)) = &entry.exact {
            if model_id == reference_model_id {
                return LayerCount { tokens: *exact_tokens, source: TokenSource::Exact };
            }
        }
    }

    let tiktoken_count = match &cached {
        Some(entry) => entry.tiktoken_count,
        None => {
            let count = tokenizer.estimate(text);
            cache.put_tiktoken(&hash, count);
            count
        }
    };

    if let Some(api_key) = api_key_store.get() {
        if let Ok(exact_tokens) = client.count_tokens(text, reference_model_id, &api_key) {
            cache.put_exact(&hash, exact_tokens, reference_model_id);
            return LayerCount { tokens: exact_tokens, source: TokenSource::Exact };
        }
    }

    match cache.calibration_factor(reference_model_id) {
        Some(factor) => {
            let scaled = (tiktoken_count as f64 * factor).round() as u32;
            LayerCount { tokens: scaled, source: TokenSource::Estimate }
        }
        None => LayerCount { tokens: tiktoken_count, source: TokenSource::Estimate },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::api_key_store::FakeApiKeyStore;
    use crate::footprint::count_tokens_client::FakeCountTokensClient;
    use crate::footprint::tokenizer::{estimate_tokens, BpeTokenizer};

    const MODEL: &str = "claude-sonnet-5";

    #[test]
    fn no_key_configured_falls_back_to_uncalibrated_estimate() {
        let cache = TokenCache::open_in_memory().unwrap();
        let api_key_store = FakeApiKeyStore::empty();
        let client = FakeCountTokensClient::always_returns(999);

        let result = count_text("some skill body text", &cache, &api_key_store, &client, &BpeTokenizer, MODEL);

        assert_eq!(result.source, TokenSource::Estimate);
        assert_eq!(result.tokens, estimate_tokens("some skill body text"));
        assert_eq!(client.call_count(), 0, "no key configured should never call the exact API");
    }

    #[test]
    fn no_key_but_prior_calibration_sample_exists_scales_the_estimate() {
        let cache = TokenCache::open_in_memory().unwrap();
        cache.put_tiktoken("prior-hash", 100);
        cache.put_exact("prior-hash", 150, MODEL); // factor = 1.5

        let api_key_store = FakeApiKeyStore::empty();
        let client = FakeCountTokensClient::always_returns(999);

        let result = count_text("some other skill body text", &cache, &api_key_store, &client, &BpeTokenizer, MODEL);
        let raw = estimate_tokens("some other skill body text");

        assert_eq!(result.source, TokenSource::Estimate);
        assert_eq!(result.tokens, (raw as f64 * 1.5).round() as u32);
    }

    #[test]
    fn key_present_and_call_succeeds_returns_exact_and_populates_cache() {
        let cache = TokenCache::open_in_memory().unwrap();
        let api_key_store = FakeApiKeyStore::with_key("sk-ant-test");
        let client = FakeCountTokensClient::always_returns(42);

        let result = count_text("skill body", &cache, &api_key_store, &client, &BpeTokenizer, MODEL);

        assert_eq!(result, LayerCount { tokens: 42, source: TokenSource::Exact });
        assert_eq!(cache.get(&sha256_hex("skill body")).unwrap().exact, Some((42, MODEL.to_string())));
    }

    #[test]
    fn a_second_call_for_identical_text_hits_the_cache_not_the_client() {
        let cache = TokenCache::open_in_memory().unwrap();
        let api_key_store = FakeApiKeyStore::with_key("sk-ant-test");
        let client = FakeCountTokensClient::always_returns(42);

        count_text("skill body", &cache, &api_key_store, &client, &BpeTokenizer, MODEL);
        let second = count_text("skill body", &cache, &api_key_store, &client, &BpeTokenizer, MODEL);

        assert_eq!(second, LayerCount { tokens: 42, source: TokenSource::Exact });
        assert_eq!(client.call_count(), 1, "second call should be served entirely from cache");
    }

    #[test]
    fn key_present_but_call_fails_falls_back_to_estimate_like_no_key() {
        let cache = TokenCache::open_in_memory().unwrap();
        let api_key_store = FakeApiKeyStore::with_key("sk-ant-test");
        let client = FakeCountTokensClient::always_fails();

        let result = count_text("skill body", &cache, &api_key_store, &client, &BpeTokenizer, MODEL);

        assert_eq!(result.source, TokenSource::Estimate);
        assert_eq!(result.tokens, estimate_tokens("skill body"));
    }

    #[test]
    fn a_cached_exact_value_under_a_stale_model_id_is_not_trusted() {
        let cache = TokenCache::open_in_memory().unwrap();
        cache.put_tiktoken(&sha256_hex("skill body"), estimate_tokens("skill body"));
        cache.put_exact(&sha256_hex("skill body"), 42, "old-model");

        let api_key_store = FakeApiKeyStore::with_key("sk-ant-test");
        let client = FakeCountTokensClient::always_returns(50);

        let result = count_text("skill body", &cache, &api_key_store, &client, &BpeTokenizer, "new-model");

        assert_eq!(result, LayerCount { tokens: 50, source: TokenSource::Exact });
        assert_eq!(client.call_count(), 1, "stale model_id should trigger a fresh call");
    }
}
