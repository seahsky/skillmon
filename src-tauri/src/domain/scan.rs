//! Scan-request threading for the windowed panel + toast surface (issue #14).
//! `ScanParams` injects "now" and the requested display window at the adapter
//! boundary; `ScanOutcome` carries the flattened report plus any toasts the
//! scan decided to fire. The `HarnessAdapter` trait stays untouched (ADR 0002):
//! these are inputs/outputs of the concrete adapter's inherent `scan`, and the
//! trait's `scan_all` is a clockless all-time shim over it.

use super::budget::ToastRequest;
use super::report::ScanReport;

pub const DAY_MILLIS: i64 = 86_400_000;
pub const HOUR_MILLIS: i64 = 3_600_000;

/// Which slice of attributed usage the panel displays. The 24h *budget* is
/// always evaluated on a fixed 24h window regardless of this (DESIGN.md UX #4);
/// this only picks the per-skill figures the rows show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindow {
    AllTime,
    Rolling { hours: u32 },
}

/// Everything time-relative a scan needs, injected at the `lib.rs` command
/// boundary so the core holds no wall-clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanParams {
    /// Wall-clock now as unix epoch millis. `0` is the `all_time()` sentinel:
    /// a clockless scan (the trait `scan_all` shim, tests) skips the
    /// time-relative 24h budget/anomaly evaluation entirely. A real scan from
    /// the panel always injects a genuine clock (always `> 0`).
    pub now_millis: i64,
    pub usage_window: UsageWindow,
}

impl ScanParams {
    /// The clockless, all-time request the trait `scan_all` delegates to.
    pub fn all_time() -> Self {
        ScanParams { now_millis: 0, usage_window: UsageWindow::AllTime }
    }
}

/// A scan's full result: the serializable report plus the toasts to emit. The
/// core has already persisted any debounce state before returning this, so a
/// dropped toast is a lost nudge, never a stuck flag (issue #14, D6).
pub struct ScanOutcome {
    pub report: ScanReport,
    pub toasts: Vec<ToastRequest>,
}
