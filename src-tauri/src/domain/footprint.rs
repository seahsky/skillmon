#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Exact,
    Estimate,
}

/// Which listing line a skill has, and therefore what its always-on layer is
/// measuring (ADR 0016). Deliberately not called a confidence: `NotListed` is
/// not a third degree of belief but the absence of the thing being judged.
///
/// Carries `Serialize` and crosses to the UI as-is, unlike `LayerCount`, which
/// `LayerReport` mirrors in order to collapse `TokenSource` to a bool. There is
/// nothing to collapse here -- the panel needs all three states -- so a mirror
/// would map each variant to an identically-named twin and earn nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AlwaysOnTextKind {
    /// The literal line a transcript shows Claude Code injected.
    Native,
    /// Built from raw frontmatter, because no transcript has listed this skill
    /// yet. A guess at a line that really is in context.
    Reconstructed,
    /// There is no line: the skill declares `disable-model-invocation: true`,
    /// so Claude Code never lists it to the model and its always-on cost is a
    /// certain zero (issue #24). Must never render like `Reconstructed` -- one
    /// is a measured absence, the other a guess at a real cost.
    NotListed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerCount {
    pub tokens: u32,
    pub source: TokenSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlwaysOnFootprint {
    pub count: LayerCount,
    pub text_kind: AlwaysOnTextKind,
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
                text_kind: AlwaysOnTextKind::Native,
            },
            on_invoke: LayerCount { tokens: 512, source: TokenSource::Estimate },
            on_demand: Some(LayerCount { tokens: 1024, source: TokenSource::Estimate }),
        };

        assert_eq!(footprint.always_on.count.tokens, 42);
        assert_eq!(footprint.always_on.text_kind, AlwaysOnTextKind::Native);
        assert_eq!(footprint.on_invoke.source, TokenSource::Estimate);
        assert_eq!(footprint.on_demand.unwrap().tokens, 1024);
    }
}
