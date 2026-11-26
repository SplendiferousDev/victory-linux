//! Switches that are off (unset or `0`), on (`1`) or in turn (`alt`:
//! twenty seconds off, twenty on, and so on, each change said in the log),
//! so that one run can compare both ways. One clock for all of them: two
//! switches in turn change together.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// A switch read from the environment variable `var` on first use; `what`
/// says in the log what it does when on.
pub(crate) struct Switch {
    var: &'static str,
    what: &'static str,
    mode: OnceLock<u8>,
    last: AtomicU8,
}

/// `0` off, `1` on, `2` in turn, from a switch's value.
pub(crate) fn mode_of(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("alt") => 2,
        _ => 0,
    }
}

/// In turn, `secs` seconds after the clock started: off for the first
/// twenty seconds, on for the next twenty, and so on.
pub(crate) fn phase_on(secs: u64) -> bool {
    (secs / 20) % 2 == 1
}

/// The start of the phases of every [`Switch`].
static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();

impl Switch {
    pub(crate) const fn new(var: &'static str, what: &'static str) -> Switch {
        Switch {
            var,
            what,
            mode: OnceLock::new(),
            last: AtomicU8::new(2),
        }
    }

    /// `0` off, `1` on, `2` in turn (read once).
    pub(crate) fn mode(&self) -> u8 {
        *self.mode.get_or_init(|| {
            let mode = mode_of(std::env::var(self.var).ok().as_deref());
            if mode == 1 {
                tracing::info!("{} ({}=1)", self.what, self.var);
            }
            mode
        })
    }

    /// Whether it is set at all (`1` or `alt`).
    pub(crate) fn set(&self) -> bool {
        self.mode() != 0
    }

    /// Whether it is on now; in turn, each change is said in the log.
    pub(crate) fn on(&self) -> bool {
        match self.mode() {
            0 => false,
            1 => true,
            _ => {
                let on = phase_on(EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs());
                if self.last.swap(u8::from(on), Ordering::Relaxed) != u8::from(on) {
                    tracing::info!("{} from now ({}=alt): {}", self.what, self.var, if on { "ON" } else { "off, as before" });
                }
                on
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_off_unless_one_or_alt() {
        assert_eq!(mode_of(None), 0);
        assert_eq!(mode_of(Some("0")), 0);
        assert_eq!(mode_of(Some("")), 0);
        assert_eq!(mode_of(Some("2")), 0);
        assert_eq!(mode_of(Some("1")), 1);
        assert_eq!(mode_of(Some(" alt ")), 2);
    }

    #[test]
    fn in_turn_twenty_seconds_off_then_twenty_on() {
        assert!(!phase_on(0));
        assert!(!phase_on(19));
        assert!(phase_on(20));
        assert!(phase_on(39));
        assert!(!phase_on(40));
        assert!(phase_on(60));
    }

    #[test]
    fn an_unset_switch_is_off() {
        static S: Switch = Switch::new("VICTORY_TEST_ALT_NEVER_SET", "nothing");
        assert!(!S.on());
        assert!(!S.set());
        assert_eq!(S.mode(), 0);
    }
}
