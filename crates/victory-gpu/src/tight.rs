//! A mode for PCs short of memory (`VICTORY_TIGHT_MEMORY=1`; off by
//! default, which keeps every default as it was).
//!
//! On a PC with 16 GB and a card with 8 GB the title's own memory (a
//! section of several GB that the system may swap) ends up in swap while
//! the layer's host copies stay resident: a buffer in a custom heap in the
//! PC's memory is memory the driver pins, so it never goes to swap, and
//! every MB of it is a MB less for the title. The windows outside buffers
//! are most of those copies (some 3.2 GB in a world, over 2.4 GB of title
//! memory, of which some 1.5 GB was ever copied), and nearly all of them
//! are held by descriptor slots, which the idle sweep must leave alone.
//!
//! The mode changes defaults only; a switch that is set keeps its value:
//! - windows outside buffers are cut on a 1 MiB grid
//!   (`VICTORY_WINDOW_KB`, 4096 otherwise): a window holds less memory
//!   nothing reads;
//! - windows go to GPU upload heaps while the card has room
//!   (`VICTORY_WINDOW_UPLOAD`, `VICTORY_CARD_WINDOWS_MB`): one buffer in the
//!   card's memory that the CPU fills, and none in the PC's; where the
//!   device has no such heaps a window is not put on the card as two
//!   buffers either (that keeps the PC's side and takes the card's room);
//! - the spare pools keep less: list scratch chunks, texture staging
//!   buffers, snapshot buffers, the threads' texture read buffers.
//!
//! The memory line ([`line`], in every build, every 10 s with the frame
//! line) says what the host copies hold by kind and where, so a run with
//! and without the mode can be compared.

use std::sync::OnceLock;

/// `VICTORY_TIGHT_MEMORY=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TIGHT_MEMORY").is_some_and(|v| v == "1"))
}

/// The window grid in KiB the mode asks for.
pub(crate) const WINDOW_KB: u64 = 1024;
/// The most MB of windows in GPU upload heaps the mode asks for (the
/// card's room rule, four fifths of its budget, comes first).
pub(crate) const CARD_WINDOWS_MB: u64 = 1536;
/// The most bytes of spare list scratch chunks the mode keeps.
pub(crate) const SCRATCH_SPARE: u64 = 32 << 20;
/// The most MB of spare texture staging buffers the mode keeps.
pub(crate) const STAGING_SPARE_MB: u64 = 32;
/// The most MB of snapshot buffers the mode keeps in the pool.
pub(crate) const SNAPSHOT_POOL_MB: u64 = 64;
/// The most MB a thread's texture read buffer keeps between textures.
pub(crate) const TEXTURE_READ_KEEP_MB: u64 = 16;

/// A number from a switch's text (spaces allowed around it).
pub(crate) fn number(text: Option<&str>) -> Option<u64> {
    text.and_then(|v| v.trim().parse::<u64>().ok())
}

/// A number switch: its value if set, else the mode's or the usual default.
pub(crate) fn pick(set: Option<u64>, tight: bool, usual: u64, tight_value: u64) -> u64 {
    set.unwrap_or(if tight { tight_value } else { usual })
}

/// An on/off switch that is on with "1": set, it decides; not set, the mode
/// does.
pub(crate) fn flag(set: Option<&str>, tight: bool) -> bool {
    match set {
        Some(v) => v == "1",
        None => tight,
    }
}

/// The value of number switch `name` in force, with the mode's default.
pub(crate) fn env_number(name: &str, usual: u64, tight_value: u64) -> u64 {
    pick(
        number(std::env::var(name).ok().as_deref()),
        on(),
        usual,
        tight_value,
    )
}

/// The window grid setting in KiB in force: `VICTORY_WINDOW_KB` if set,
/// the mode's grid with the mode, none otherwise (the usual 4 MiB).
pub(crate) fn window_kb(set: Option<u64>, tight: bool) -> Option<u64> {
    set.or(tight.then_some(WINDOW_KB))
}

/// Whether a window the GPU upload heaps cannot take may go to the card as
/// two buffers (the card's side, and the CPU's in the PC's memory): not in
/// the mode unless `VICTORY_CARD_WINDOWS_MB` was set, since such a window
/// saves none of the PC's memory and takes the card's room the upload
/// heaps would use.
pub(crate) fn plain_card_windows(tight: bool, card_windows_set: bool) -> bool {
    !tight || card_windows_set
}

/// Bytes in MB, for the lines.
fn mb(bytes: u64) -> u64 {
    bytes >> 20
}

/// What the host copies and pools hold, as [`line`] says it (bytes).
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    /// Windows outside buffers: in the PC's memory (pinned), in GPU upload
    /// heaps (the card's), with a side on the card and one in the PC's.
    pub windows_pc: [u64; 2],
    pub windows_upload: [u64; 2],
    pub windows_card: [u64; 2],
    /// The 4 MiB cells the windows alive lie in, in bytes: about what they
    /// would take on the usual grid.
    pub windows_cells: u64,
    /// Windows that only descriptor slots hold (no list, no pass) and
    /// nothing asked for in 30 s: what a sweep could take if slots let go.
    pub windows_slot_idle: [u64; 2],
    /// Title buffers' copies: all, and of them the CPU sides of copies on
    /// the card (the card's sides are the card's memory).
    pub buffers: [u64; 2],
    pub buffers_card: [u64; 2],
}

impl Held {
    /// The PC's memory the copies hold (pinned by the driver).
    pub(crate) fn pc_bytes(&self) -> u64 {
        self.windows_pc[1] + self.windows_card[1] + self.buffers[1]
    }
}

/// The memory line's text for `held`, the pools (scratch chunks and their
/// spare part, texture staging alive and spare), the PC's available
/// memory and the card's [budget, used, shared budget, shared used].
pub(crate) fn text(
    held: &Held,
    pools: [u64; 4],
    available: u64,
    card: Option<[u64; 4]>,
    tight: bool,
) -> String {
    let card = card.map_or_else(
        || "unknown".to_string(),
        |c| {
            format!(
                "{} of {} MB in use, the PC's memory shared with it {} MB in use",
                mb(c[1]),
                mb(c[0]),
                mb(c[3])
            )
        },
    );
    format!(
        "memory of host copies (PC memory the driver pins, unless on the card): {} MB in the PC's memory; windows \
         outside buffers: {} in the PC's memory {} MB, {} in GPU upload heaps {} MB (the card's), {} with a card side \
         {} MB (and as much on the card); their 4 MiB cells: {} MB; held by descriptor slots alone and not asked for \
         in 30 s: {} {} MB; title buffers' copies: {} {} MB, of them {} CPU sides of copies on the card {} MB; list \
         scratch {} MB ({} MB spare); texture staging {} MB ({} MB spare); the PC has {} MB available; the card: {}; \
         VICTORY_TIGHT_MEMORY {}",
        mb(held.pc_bytes()),
        held.windows_pc[0],
        mb(held.windows_pc[1]),
        held.windows_upload[0],
        mb(held.windows_upload[1]),
        held.windows_card[0],
        mb(held.windows_card[1]),
        mb(held.windows_cells),
        held.windows_slot_idle[0],
        mb(held.windows_slot_idle[1]),
        held.buffers[0],
        mb(held.buffers[1]),
        held.buffers_card[0],
        mb(held.buffers_card[1]),
        mb(pools[0]),
        mb(pools[1]),
        mb(pools[2]),
        mb(pools[3]),
        mb(available),
        card,
        if tight { "on" } else { "off" },
    )
}

/// Every 10 s, with the frame line: the memory line.
#[cfg(windows)]
pub(crate) fn line() {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let last = LAST.load(Relaxed);
    if (last != 0 && now < last + 10_000)
        || LAST
            .compare_exchange(last, now.max(1), Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let held = crate::shadow::held();
    let pools = [
        crate::host::ALIVE[2].load(Relaxed),
        crate::host::ALIVE[4].load(Relaxed),
        crate::texture::staged_bytes(),
        crate::texture::spare_bytes(),
    ];
    let card = crate::host::host().and_then(|h| h.video_memory());
    tracing::info!(
        "{}",
        text(&held, pools, crate::framelog::free_mb() << 20, card, on())
    );
}

/// Says once what the mode set, with the line's first appearance.
#[cfg(windows)]
pub(crate) fn say() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if !on() {
        return;
    }
    ONCE.call_once(|| {
        tracing::info!(
            "VICTORY_TIGHT_MEMORY=1: defaults for a PC short of memory, where not set: VICTORY_WINDOW_KB={WINDOW_KB} \
             VICTORY_WINDOW_UPLOAD=1 VICTORY_CARD_WINDOWS_MB={CARD_WINDOWS_MB} (no windows on the card beside a copy \
             in the PC's memory) VICTORY_STAGING_POOL_MB={STAGING_SPARE_MB} VICTORY_SNAPSHOT_POOL_MB={SNAPSHOT_POOL_MB} \
             VICTORY_TEXTURE_READ_KEEP_MB={TEXTURE_READ_KEEP_MB}; list scratch kept spare: {} MB",
            SCRATCH_SPARE >> 20
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_switch_wins_over_the_mode() {
        assert_eq!(pick(Some(7), true, 1, 2), 7);
        assert_eq!(pick(Some(7), false, 1, 2), 7);
        assert_eq!(pick(Some(0), true, 1, 2), 0);
        assert_eq!(pick(None, true, 1, 2), 2);
        assert_eq!(pick(None, false, 1, 2), 1);
    }

    #[test]
    fn flags_follow_the_mode_only_when_not_set() {
        assert!(flag(None, true));
        assert!(!flag(None, false));
        assert!(flag(Some("1"), false));
        assert!(!flag(Some("0"), true));
        assert!(!flag(Some("yes"), true));
    }

    #[test]
    fn numbers_take_spaces_and_refuse_text() {
        assert_eq!(number(Some(" 256 ")), Some(256));
        assert_eq!(number(Some("x")), None);
        assert_eq!(number(None), None);
    }

    #[test]
    fn the_grid_is_finer_only_in_the_mode() {
        assert_eq!(window_kb(None, false), None);
        assert_eq!(window_kb(None, true), Some(WINDOW_KB));
        assert_eq!(window_kb(Some(4096), true), Some(4096));
        assert_eq!(window_kb(Some(256), false), Some(256));
    }

    #[test]
    fn no_plain_card_windows_in_the_mode_unless_asked() {
        assert!(plain_card_windows(false, false));
        assert!(plain_card_windows(false, true));
        assert!(!plain_card_windows(true, false));
        assert!(plain_card_windows(true, true));
    }

    #[test]
    fn the_line_counts_pc_memory_but_not_the_cards() {
        let held = Held {
            windows_pc: [3, 30 << 20],
            windows_upload: [2, 200 << 20],
            windows_card: [1, 4 << 20],
            windows_cells: 64 << 20,
            windows_slot_idle: [1, 8 << 20],
            buffers: [5, 50 << 20],
            buffers_card: [1, 40 << 20],
        };
        // Upload heaps are the card's; a card window's CPU side and every
        // title buffer's copy (the CPU sides of card copies among them) are
        // the PC's.
        assert_eq!(held.pc_bytes(), (30 + 4 + 50) << 20);
        let line = text(
            &held,
            [8 << 20, 4 << 20, 2 << 20, 1 << 20],
            900 << 20,
            Some([6000 << 20, 4000 << 20, 0, 3000 << 20]),
            true,
        );
        assert!(
            line.starts_with(
                "memory of host copies (PC memory the driver pins, unless on the card): 84 MB"
            ),
            "{line}"
        );
        assert!(line.contains("2 in GPU upload heaps 200 MB"), "{line}");
        assert!(line.contains("the PC has 900 MB available"), "{line}");
        assert!(line.contains("4000 of 6000 MB in use"), "{line}");
        assert!(line.ends_with("VICTORY_TIGHT_MEMORY on"), "{line}");
        assert!(text(&Held::default(), [0; 4], 0, None, false).contains("the card: unknown"));
    }
}
