use crate::domain::skill::Frontmatter;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrontmatterError {
    #[error("no frontmatter delimiters found")]
    NoDelimiters,
    #[error("invalid YAML frontmatter: {0}")]
    InvalidYaml(#[from] serde_yaml_ng::Error),
}

#[derive(Debug, serde::Deserialize)]
struct FrontmatterYaml {
    name: String,
    description: String,
    /// Claude Code drops a skill declaring this from the model-facing skill
    /// listing entirely, so it costs zero always-on tokens while staying
    /// slash-invokable (issue #24). Absent means listed, hence the default.
    #[serde(rename = "disable-model-invocation", default)]
    disable_model_invocation: bool,
}

pub fn parse_skill_md(content: &str) -> Result<(Frontmatter, String), FrontmatterError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content
        .strip_prefix("---\r\n")
        .or_else(|| content.strip_prefix("---\n"))
        .ok_or(FrontmatterError::NoDelimiters)?;
    let end = rest.find("\n---").ok_or(FrontmatterError::NoDelimiters)?;
    let raw_block = &rest[..end];
    let after = &rest[end + 4..];
    let body = after
        .strip_prefix("\n\n")
        .or_else(|| after.strip_prefix('\n'))
        .unwrap_or(after)
        .to_string();

    let parsed: FrontmatterYaml = serde_yaml_ng::from_str(raw_block)?;

    Ok((
        Frontmatter {
            declared_name: parsed.name,
            description: parsed.description,
            raw_block: raw_block.to_string(),
            model_invocable: !parsed.disable_model_invocation,
        },
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_description_and_body() {
        let content = "---\nname: grilling\ndescription: Interview the user relentlessly.\n---\n\nBody line one.\nBody line two.\n";
        let (frontmatter, body) = parse_skill_md(content).unwrap();
        assert_eq!(frontmatter.declared_name, "grilling");
        assert_eq!(frontmatter.description, "Interview the user relentlessly.");
        assert_eq!(body, "Body line one.\nBody line two.\n");
    }

    #[test]
    fn tolerates_unknown_extra_frontmatter_fields() {
        let content = "---\nname: gstack-ship\ndescription: ships it\npreamble-tier: 4\nallowed-tools: Bash\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill_md(content).unwrap();
        assert_eq!(frontmatter.declared_name, "gstack-ship");
    }

    #[test]
    fn skills_are_model_invocable_by_default() {
        let content = "---\nname: grilling\ndescription: grills\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill_md(content).unwrap();
        assert!(frontmatter.model_invocable);
    }

    #[test]
    fn disable_model_invocation_true_makes_a_skill_not_model_invocable() {
        let content = "---\nname: grill-with-docs\ndescription: sharpens a plan\ndisable-model-invocation: true\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill_md(content).unwrap();
        assert!(!frontmatter.model_invocable);
    }

    #[test]
    fn disable_model_invocation_false_keeps_a_skill_model_invocable() {
        let content = "---\nname: grilling\ndescription: grills\ndisable-model-invocation: false\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill_md(content).unwrap();
        assert!(frontmatter.model_invocable);
    }

    #[test]
    fn errors_on_missing_delimiters() {
        let content = "no frontmatter here";
        assert!(matches!(parse_skill_md(content), Err(FrontmatterError::NoDelimiters)));
    }

    #[test]
    fn errors_on_missing_required_field() {
        let content = "---\nname: incomplete\n---\n\nBody.\n";
        assert!(matches!(parse_skill_md(content), Err(FrontmatterError::InvalidYaml(_))));
    }

    #[test]
    fn colon_in_description_does_not_break_parsing() {
        let content = "---\nname: codex\ndescription: \"Ask codex anything: second opinions welcome\"\n---\n\nBody.\n";
        let (frontmatter, _) = parse_skill_md(content).unwrap();
        assert_eq!(frontmatter.description, "Ask codex anything: second opinions welcome");
    }
}
