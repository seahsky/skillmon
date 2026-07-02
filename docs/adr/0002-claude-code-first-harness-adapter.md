# 2. Claude Code first, behind a harness-adapter trait

## Status

Accepted.

## Context

The skill/plugin ecosystem skillmon monitors is currently Claude Code's, with a specific and evolving on-disk layout (`~/.claude/skills`, `plugins/cache`, `installed_plugins.json`, `settings.json`, JSONL transcripts).
Other agents may adopt similar concepts later, but building a generic abstraction now would be speculative.

## Decision

Ship one concrete adapter for Claude Code, but put every agent-specific concern behind a harness-adapter trait.

The trait abstracts: skill discovery (where and how deep to scan), footprint sources (which files make up a skill), transcript location and schema (for attribution), and mutation ops (how to enable/disable/uninstall). v1 has exactly one implementation.

## Consequences

- No feature waits on a second adapter; the trait is a boundary, not a framework.
- Claude-specific facts (depth-1 personal-skill scan, `plugin.json.skills` relocation, `enabledPlugins` keys, `message.id` dedup) live inside the adapter, not leaked into UI or core.
- The trait shape is validated against exactly one implementation, so it will need revision when a second agent lands; that is acceptable and expected (YAGNI over premature generality).

## Options considered

- **Hardcode Claude Code with no boundary** — fastest, but bakes Claude paths through the whole core; rejected.
- **Design a full multi-harness plugin system now** — speculative generality with one known consumer; rejected.
- **Single trait, one implementation** — chosen.
