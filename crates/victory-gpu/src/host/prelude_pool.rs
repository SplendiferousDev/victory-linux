//! Host lists' preludes from one shared pool (`VICTORY_PRELUDE_POOL=1`; off
//! by default). Apart from `host.rs` for the asset policy's size limit of a
//! source file.
//!
//! A prelude (an allocator and a list: the texture moves, twin copies,
//! detiles and gathered-record clamps a host list needs just before it) is
//! recorded when its list is submitted and runs in that one submission.
//! Each host list kept one of its own for as long as it lived, made at its
//! first need ([`lazy_prelude`]) and never let go: the title's ~630 lists
//! and up to 512 idle ones, each holding an allocator whose driver memory
//! never shrinks (vkd3d-proton resets its command pools without releasing
//! their memory).
//!
//! With the switch a prelude is taken from the pool when a submission
//! records one, and handed back through [`retire`] once recorded: the
//! retired path keeps it until every submission started by then has
//! finished ([`bury`]), and only then does it return to the pool, so a
//! prelude is never reset while a submission that runs it may still be on
//! the card. Past [`IDLE_MOST`] idle ones it is let go instead.
//!
//! Idea from skornuta, victory-memory-stack (shared prelude pool).

use super::*;

/// The most idle preludes kept.
const IDLE_MOST: usize = 64;

/// Preludes whose last submission has finished, ready to record.
static FREE: Mutex<Vec<Prelude>> = Mutex::new(Vec::new());

/// The pool's preludes alive, made, and taken again since the start.
pub(crate) static STATS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether preludes come from the pool (`VICTORY_PRELUDE_POOL=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_PRELUDE_POOL").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                "host lists' preludes come from one shared pool (VICTORY_PRELUDE_POOL=1): each goes back once its submission has finished; at most {IDLE_MOST} idle"
            );
        }
        on
    })
}

/// A prelude whose last submission has finished, or a new one.
pub(super) fn take(host: Option<&Host>) -> Option<Prelude> {
    let kept = crate::lockmeter::lock("ho:PRELUDE_POOL", &FREE).pop();
    if let Some(prelude) = kept {
        STATS[2].fetch_add(1, Ordering::Relaxed);
        return Some(prelude);
    }
    let made = host?.make_prelude()?;
    STATS[0].fetch_add(1, Ordering::Relaxed);
    STATS[1].fetch_add(1, Ordering::Relaxed);
    Some(made)
}

/// A prelude just recorded for the submission being prepared: back to the
/// pool when every submission started so far has finished.
pub(super) fn give(prelude: Prelude) {
    retire(Back(Some(prelude)));
}

/// What [`retire`] holds: dropped only once the submissions that may run
/// the prelude have finished, and then the prelude goes back to the pool.
struct Back(Option<Prelude>);

impl Drop for Back {
    fn drop(&mut self) {
        let Some(prelude) = self.0.take() else {
            return;
        };
        let surplus = {
            let mut free = crate::lockmeter::lock("ho:PRELUDE_POOL", &FREE);
            if free.len() < IDLE_MOST {
                free.push(prelude);
                None
            } else {
                Some(prelude)
            }
        };
        if let Some(prelude) = surplus {
            STATS[0].fetch_sub(1, Ordering::Relaxed);
            // Let go outside the lock; its submissions have finished.
            drop(prelude);
        }
    }
}

/// For the memory-by-site line: the pool's figures, or nothing when off.
pub(crate) fn text() -> String {
    if !on() {
        return String::new();
    }
    format!(
        "; prelude pool (VICTORY_PRELUDE_POOL=1): {} alive, {} idle, {} made, {} taken again",
        STATS[0].load(Ordering::Relaxed),
        crate::lockmeter::lock("ho:PRELUDE_POOL", &FREE).len(),
        STATS[1].load(Ordering::Relaxed),
        STATS[2].load(Ordering::Relaxed)
    )
}
