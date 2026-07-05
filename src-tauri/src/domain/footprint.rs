#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Exact,
    Estimate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextConfidence {
    Native,
    Reconstructed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerCount {
    pub tokens: u32,
    pub source: TokenSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlwaysOnFootprint {
    pub count: LayerCount,
    pub confidence: TextConfidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    pub always_on: AlwaysOnFootprint,
    pub on_invoke: LayerCount,
    /// `None` means the on-demand ceiling is still pending: the interactive
    /// scan deferred its tokenization and a background pass has not filled it
    /// yet (issue #11). `Some(LayerCount { tokens: 0, .. })` is the distinct
    /// "resolved, and there is nothing to load" state for a skill with no
    /// bundled files -- never conflated with pending.
    pub on_demand: Option<LayerCount>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_a_footprint_with_three_layers() {
        let footprint = Footprint {
            always_on: AlwaysOnFootprint {
                count: LayerCount { tokens: 42, source: TokenSource::Exact },
                confidence: TextConfidence::Native,
            },
            on_invoke: LayerCount { tokens: 512, source: TokenSource::Estimate },
            on_demand: Some(LayerCount { tokens: 1024, source: TokenSource::Estimate }),
        };

        assert_eq!(footprint.always_on.count.tokens, 42);
        assert_eq!(footprint.always_on.confidence, TextConfidence::Native);
        assert_eq!(footprint.on_invoke.source, TokenSource::Estimate);
        assert_eq!(footprint.on_demand.unwrap().tokens, 1024);
    }
}
