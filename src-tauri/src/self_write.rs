//! Telling skillmon's own writes apart from everyone else's (issue #29).
//!
//! Every reversible removal moves entries *out of* the scan root and every
//! restore moves them back *into* it -- and that root is watched **recursively**
//! (ADR 0019). So skillmon's own mutations trip skillmon's own watcher. A tool
//! uninstall moves 47 entries (ADR 0027), so it trips it mid-cascade, while the
//! ledger recording the unit has not committed yet and the tree on disk is
//! genuinely half-removed.
//!
//! The latch here is what a watcher consults before turning a filesystem event
//! into a rescan, and what a mutation holds across its writes. It is
//! harness-neutral on purpose (ADR 0002): its users already span
//! `adapters/claude_code/` (the watcher, and the plugin-CLI mutations to come)
//! and `removal/`'s callers, and a second harness would want the same mechanism
//! rather than a copy of it.
//!
//! **It is a latch, not an attribution.** It cannot say *which* write an event
//! came from, only that skillmon was writing when it happened -- see `is_open`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Open while skillmon is writing inside a watched tree, and for a `tail`
/// afterwards.
///
/// Clone to share: every clone is a handle on the same latch, not a new one.
#[derive(Clone)]
pub struct SelfWriteWindow(Arc<Inner>);

struct Inner {
    /// How many mutations are writing *right now*. A counter, not a flag,
    /// because a long removal and a restore can be in flight together -- and
    /// with a flag the first to finish would close the latch out from under the
    /// other. It also keeps a slow mutation (a cross-device copy of a 1.1 GB
    /// checkout) suppressed for its whole duration, however long that is, which
    /// no fixed duration could promise.
    depth: AtomicUsize,
    /// When the last finished mutation's tail expires. `Instant`, not the
    /// injected `now_millis` the scan core is clockless about (CLAUDE.md): this
    /// is event-delivery timing, in the same category as `toggle_panel`'s
    /// double-toggle guard, and it is compared against nothing the domain ever
    /// sees.
    quiet_until: Mutex<Instant>,
    tail: Duration,
}

impl SelfWriteWindow {
    /// `tail` is how long suppression must outlive the write itself. Sizing it
    /// is the caller's business, because only the watcher knows its own
    /// debounce -- see `adapters::claude_code::watcher::SELF_WRITE_TAIL`.
    pub fn new(tail: Duration) -> Self {
        Self(Arc::new(Inner {
            depth: AtomicUsize::new(0),
            // In the past already, so a window nobody has written through yet is
            // closed rather than briefly swallowing a real change at startup.
            quiet_until: Mutex::new(Instant::now()),
            tail,
        }))
    }

    /// Marks skillmon as writing until the returned guard drops -- and then for
    /// the tail beyond it.
    #[must_use = "the window closes the moment the guard drops, so a guard that is not held suppresses nothing"]
    pub fn open(&self) -> SelfWriteGuard {
        self.0.depth.fetch_add(1, Ordering::SeqCst);
        SelfWriteGuard(self.0.clone())
    }

    /// Whether an event arriving *now* is attributable to skillmon.
    ///
    /// Deliberately coarse, and it costs a real thing: an *external* change
    /// landing inside the window is attributed to skillmon and dropped with the
    /// rest. That is the honest trade, not an oversight (ADR 0019 Update 4).
    /// Per-path attribution would be finer but not actually reliable -- a move
    /// out of a directory also fires an event on the directory, and
    /// `~/.claude/skills` is the parent of every skill, so suppressing it would
    /// swallow the very changes the watcher exists for.
    pub fn is_open(&self) -> bool {
        self.0.depth.load(Ordering::SeqCst) > 0
            || Instant::now() < *self.0.quiet_until.lock().expect("self-write window mutex poisoned")
    }
}

/// Held across a mutation's writes. Drop-driven so an early `?` return, or a
/// panic unwinding out of a mutation, cannot leave the watcher permanently deaf.
pub struct SelfWriteGuard(Arc<Inner>);

impl Drop for SelfWriteGuard {
    fn drop(&mut self) {
        // Arm the tail BEFORE dropping the depth, never after. Between those two
        // statements the latch is held open by neither, and the whole reason
        // this type exists is that the event to be swallowed has not been
        // delivered yet -- so that gap is exactly where it would slip through.
        *self.0.quiet_until.lock().expect("self-write window mutex poisoned") = Instant::now() + self.0.tail;
        self.0.depth.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tails that make every case below deterministic, with no sleeping
    /// and nothing to flake: `ZERO` expires the instant it is armed, and `NEVER`
    /// outlives any test run. The real tail's *duration* is the watcher's to
    /// choose; what is asserted here is the ordering around it.
    const ZERO: Duration = Duration::ZERO;
    const NEVER: Duration = Duration::from_secs(86_400);

    #[test]
    fn a_fresh_window_is_closed() {
        assert!(!SelfWriteWindow::new(NEVER).is_open(), "startup must not swallow a real change");
    }

    #[test]
    fn the_window_is_open_while_a_guard_is_held() {
        let window = SelfWriteWindow::new(ZERO);
        let _writing = window.open();
        assert!(window.is_open());
    }

    /// The point of the tail. A guard is released when `rename(2)` returns, but
    /// the event it raised is delivered a debounce window later, so a latch that
    /// closed on drop would suppress nothing at all.
    #[test]
    fn the_window_stays_open_after_the_guard_drops() {
        let window = SelfWriteWindow::new(NEVER);
        drop(window.open());
        assert!(window.is_open(), "the write's event has not even been delivered yet");
    }

    #[test]
    fn the_window_closes_once_the_tail_expires() {
        let window = SelfWriteWindow::new(ZERO);
        drop(window.open());
        assert!(!window.is_open(), "suppression is a window, not a switch someone has to remember to flip back");
    }

    /// A restore finishing mid-uninstall must not un-suppress the uninstall.
    #[test]
    fn concurrent_mutations_hold_the_window_open_until_the_last_one_finishes() {
        let window = SelfWriteWindow::new(ZERO);
        let outer = window.open();
        let inner = window.open();

        drop(inner);
        assert!(window.is_open(), "the other mutation is still writing");

        drop(outer);
        assert!(!window.is_open());
    }

    /// Clones are handles on one latch, not independent ones -- the watcher and
    /// every mutation each hold their own.
    #[test]
    fn a_clone_observes_the_same_latch() {
        let window = SelfWriteWindow::new(ZERO);
        let watcher_side = window.clone();
        let _writing = window.open();
        assert!(watcher_side.is_open());
    }

    /// The guard is what a mutation carries into `spawn_blocking`, so it has to
    /// cross a thread boundary. A compile-time assertion, not a runtime one.
    #[test]
    fn the_guard_is_send_so_a_mutation_can_hold_it_on_the_blocking_pool() {
        fn assert_send<T: Send>() {}
        assert_send::<SelfWriteGuard>();
        assert_send::<SelfWriteWindow>();
    }
}
