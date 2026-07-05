//! Pure budget + anomaly evaluation for the rolling-24h toast surface (issue
//! #14, ADR 0025). No I/O, no wall-clock, no `AppHandle`: "now" is injected as
//! a cutoff at the adapter boundary, the persisted debounce flag is passed in
//! and returned, and emission happens in `lib.rs`. Everything here is a pure
//! function of its arguments so the whole toast policy is unit-testable.

/// The default 24h attributed-work budget: a product default (DESIGN.md UX #4),
/// user-configurable and on by default. Deliberately measured against
/// *attributed* work (the sum over `message_usage`, which holds only
/// skill-attributed rows), NOT all work: the all-work median is far higher and
/// would spam. 250k is a sane ceiling for "tokens spent while skills were
/// active" in a heavy day.
pub const DEFAULT_BUDGET_WORK_TOKENS: u64 = 250_000;

/// A skill running this many times its trailing daily average trips an anomaly
/// toast (off by default, DESIGN.md UX #4).
pub const DEFAULT_ANOMALY_MULTIPLIER: f64 = 3.0;

/// Below this many work tokens a spike is ignored: a jump from 2 to 20 tokens
/// is a 10x multiple but not worth a toast.
pub const DEFAULT_ANOMALY_FLOOR: u64 = 10_000;

/// Total day span the anomaly scan looks back over: the current day plus seven
/// trailing days.
pub const ANOMALY_WINDOW_DAYS: i64 = 8;

/// The budget policy inputs `evaluate_budget` needs, resolved from `usage_meta`
/// at the adapter boundary. Distinct from the IPC `UsageSettings` DTO: this is
/// only what the budget check itself consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetConfig {
    pub enabled: bool,
    pub work_token_limit: u64,
}

/// A toast the core decided should fire; `lib.rs` turns it into copy and shows
/// it. Kept as data (not a rendered string) so the pure core never touches the
/// notification API and the copy stays unit-testable via `copy()`.
#[derive(Debug, Clone, PartialEq)]
pub enum ToastRequest {
    /// The rolling-24h attributed work crossed the configured budget.
    Budget { rolling_work: u64, limit: u64 },
    /// A single skill spiked above `multiple`x its trailing daily average.
    Anomaly { skill: String, window_work: u64, multiple: f64 },
}

/// Title + body for a system notification. No `$`, no em dash, always framed as
/// an estimate of tokens spent *during* a skill, never *by* it (ADR 0003).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastCopy {
    pub title: String,
    pub body: String,
}

impl ToastRequest {
    pub fn copy(&self) -> ToastCopy {
        match self {
            ToastRequest::Budget { rolling_work, limit } => ToastCopy {
                title: "Usage budget exceeded".to_string(),
                body: format!(
                    "~{} work tokens estimated during your skills in the last 24h, over your ~{} budget. \
                     This is an estimate of tokens spent while skills were active, not a bill.",
                    compact_tokens(*rolling_work),
                    compact_tokens(*limit),
                ),
            },
            ToastRequest::Anomaly { skill, window_work, multiple } => ToastCopy {
                title: format!("Unusual usage during {skill}"),
                body: format!(
                    "~{} work tokens estimated during {skill} today, about {:.1}x its recent daily average. \
                     An estimate of tokens spent while the skill was active, not by it.",
                    compact_tokens(*window_work),
                    multiple,
                ),
            },
        }
    }
}

/// The result of one budget evaluation: whether to toast, and the debounce flag
/// to persist. `next_alerted` is always returned so the caller writes it back
/// unconditionally (idempotent).
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetOutcome {
    pub toast: Option<ToastRequest>,
    pub next_alerted: bool,
}

/// Fire-once-on-crossing with a persisted debounce. Strict `>`: exactly at the
/// limit does not fire. While already alerted, stay silent. At or under the
/// limit, re-arm (`next_alerted = false`) so the next crossing toasts again;
/// the rolling-24h figure falls on its own as messages age out, so this reset
/// happens naturally and survives restart via `usage_meta`. Disabled never
/// toasts and leaves the persisted flag untouched.
pub fn evaluate_budget(rolling_work: u64, cfg: &BudgetConfig, alerted: bool) -> BudgetOutcome {
    if !cfg.enabled {
        return BudgetOutcome { toast: None, next_alerted: alerted };
    }
    if rolling_work > cfg.work_token_limit {
        if alerted {
            BudgetOutcome { toast: None, next_alerted: true }
        } else {
            BudgetOutcome {
                toast: Some(ToastRequest::Budget { rolling_work, limit: cfg.work_token_limit }),
                next_alerted: true,
            }
        }
    } else {
        BudgetOutcome { toast: None, next_alerted: false }
    }
}

/// How many times over its trailing daily average a skill is running, or `None`
/// when it is not anomalous. `None` with no trailing history (nothing to
/// compare against) or below `floor` (too small to matter). Strict `>` against
/// `mult * mean`.
pub fn detect_anomaly(current: u64, trailing: &[u64], mult: f64, floor: u64) -> Option<f64> {
    if trailing.is_empty() || current < floor {
        return None;
    }
    let sum: u64 = trailing.iter().copied().sum();
    let mean = sum as f64 / trailing.len() as f64;
    if mean > 0.0 && (current as f64) > mult * mean {
        Some(current as f64 / mean)
    } else {
        None
    }
}

/// Compact token count for toast copy: `250k`, `1.5M`. Byte-for-byte the same
/// shape as the panel's TS `compactTokens`, so the toast and the panel read
/// consistently.
fn compact_tokens(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let k = n as f64 / 1000.0;
        return if n < 10_000 { format!("{k:.1}k") } else { format!("{k:.0}k") };
    }
    format!("{:.1}M", n as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, limit: u64) -> BudgetConfig {
        BudgetConfig { enabled, work_token_limit: limit }
    }

    #[test]
    fn budget_disabled_never_toasts() {
        let out = evaluate_budget(1_000_000, &cfg(false, 250_000), false);
        assert!(out.toast.is_none(), "a disabled budget never toasts, even far over the limit");
        assert!(!out.next_alerted, "a disabled budget leaves the flag as passed in");
    }

    #[test]
    fn budget_fires_once_when_crossed_then_stays_silent_until_reset() {
        let c = cfg(true, 250_000);

        // 1. under limit, not yet alerted -> silent, stays un-armed.
        let s1 = evaluate_budget(100_000, &c, false);
        assert!(s1.toast.is_none());
        assert!(!s1.next_alerted);

        // 2. crosses the limit -> fires once, arms the debounce.
        let s2 = evaluate_budget(300_000, &c, s1.next_alerted);
        assert!(matches!(s2.toast, Some(ToastRequest::Budget { rolling_work: 300_000, limit: 250_000 })));
        assert!(s2.next_alerted);

        // 3. still over the limit but already alerted -> debounced, no re-toast.
        let s3 = evaluate_budget(320_000, &c, s2.next_alerted);
        assert!(s3.toast.is_none(), "still over the limit must not re-toast while alerted");
        assert!(s3.next_alerted);

        // 4. window ages back under the limit -> re-arms for the next crossing.
        let s4 = evaluate_budget(90_000, &c, s3.next_alerted);
        assert!(s4.toast.is_none());
        assert!(!s4.next_alerted, "back under the limit must reset the debounce");
    }

    #[test]
    fn budget_exactly_at_limit_does_not_fire() {
        let out = evaluate_budget(250_000, &cfg(true, 250_000), false);
        assert!(out.toast.is_none(), "strict >: exactly at the limit is not over");
        assert!(!out.next_alerted);
    }

    #[test]
    fn budget_toast_copy_is_estimate_framed_no_dollars() {
        let copy = ToastRequest::Budget { rolling_work: 300_000, limit: 250_000 }.copy();
        assert!(!copy.body.contains('$'), "no dollar values anywhere (ADR 0003)");
        assert!(!copy.body.contains('—') && !copy.title.contains('—'), "no em dashes (Sky's rule)");
        assert!(copy.body.contains('~'), "usage stays a `~` estimate");
        assert!(copy.body.contains("estimate"), "must name itself an estimate");
        assert!(copy.body.contains("during"), "framed as tokens during skills, not by them");
        assert!(copy.body.contains("not a bill"));
        assert!(!copy.body.to_lowercase().contains("used by"), "never claims tokens were used by the skill");
        // Compact figures, not raw counts, and the title reads as a problem.
        assert!(copy.body.contains("300k") && copy.body.contains("250k"));
        assert!(copy.title.contains("exceeded"));
    }

    #[test]
    fn detect_anomaly_flags_above_multiplier() {
        // mean of [100,120,80] = 100; 1000 > 3*100 and 1000 >= floor 200.
        let m = detect_anomaly(1000, &[100, 120, 80], 3.0, 200).unwrap();
        assert!((m - 10.0).abs() < 1e-9, "1000 / mean 100 = 10x");
    }

    #[test]
    fn detect_anomaly_respects_floor_and_multiplier() {
        // Below the floor: a 10x jump from tiny numbers is not worth a toast.
        assert!(detect_anomaly(150, &[10, 20, 15], 3.0, 10_000).is_none(), "under floor -> None");
        // Above the floor but only ~1.2x the mean: not anomalous.
        assert!(detect_anomaly(12_000, &[10_000, 11_000, 9_000], 3.0, 10_000).is_none(), "not over the multiplier");
    }

    #[test]
    fn detect_anomaly_needs_trailing_history() {
        assert!(detect_anomaly(1_000_000, &[], 3.0, 10_000).is_none(), "no history -> nothing to compare against");
    }

    #[test]
    fn anomaly_toast_copy_is_estimate_framed_and_names_the_skill() {
        let copy = ToastRequest::Anomaly { skill: "grilling".to_string(), window_work: 120_000, multiple: 4.2 }.copy();
        assert!(copy.title.contains("grilling") && copy.body.contains("grilling"), "must name the skill");
        assert!(!copy.body.contains('$'), "no dollar values");
        assert!(!copy.body.contains('—') && !copy.title.contains('—'), "no em dashes");
        assert!(copy.body.contains('~') && copy.body.contains("estimate"), "framed as a `~` estimate");
        assert!(copy.body.contains("not by it"), "tokens during the skill, not by it (ADR 0003)");
    }
}
