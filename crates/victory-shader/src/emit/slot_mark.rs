//! Slot marks (`VICTORY_SLOT_FEEDBACK=1`; off by default): the translated
//! programs note which view heap slots they take textures from.
//!
//! A program reads most of the title's textures through a view heap slot it
//! picks by an index it computes; nothing on the CPU says whether a frame
//! read one, so the host cannot tell a texture in use from one that only
//! lies in the heap. With the switch every texture taken from the view heap
//! at a place the host cannot know (anything but a fixed place of a table a
//! used-resource entry names, which the draw that binds the table brings up
//! to date) stores 1 into dword `index` of a raw buffer the host keeps at
//! [`mark_slot`], the slot below the null slot. The host copies the buffer
//! down and clears it every few frames, and so knows the slots read.
//!
//! The store is a plain one: every lane stores the same value, so there is
//! nothing to read first, nothing to wait for and no branch, and a store
//! lost to the host's clear only makes a slot look unread for one copy.
//! The index is the clamped one the handle is made from, so the store stays
//! inside the buffer (one dword per slot up to and with the null slot).
//!
//! Pixel programs: a store makes a pixel program one with an effect besides
//! its outputs, and a host then shades the pixels the depth test would have
//! thrown away. Where it changes nothing the host asks for the form that
//! tests depth and stencil first ([`with_early_depth`], the DXIL flag
//! `ForceEarlyDepthStencil`): [`EARLY_OK`] and [`DISCARDS`] tell it of which
//! programs it may. `VICTORY_SLOT_FEEDBACK_PIXEL`: unset or `all` every
//! pixel program marks; `free` only those that lose nothing by it (ones
//! with other stores already, and ones that neither discard nor write
//! depth, stencil or coverage: the host tests those early); `none` no pixel
//! program marks. With `free` and `none` the marks do not cover every read.
//!
//! `VICTORY_SLOT_FEEDBACK_TABLES=1`: fixed table places are marked too.
//!
//! Nothing of this is in a translation while the switch is off. The
//! switches are part of the translation's fingerprint (their names are
//! string literals of this crate, `build.rs`), so the shader cache keeps
//! such translations apart from the usual ones.
//!
//! The idea of having shaders note the heap slots they read, for texture
//! eviction, is from skornuta's memory stack for VIctory (7c274de).

use std::sync::OnceLock;

/// `PRIV` part of a container with marks: this tag, then [`MARKS`] and the
/// other bits in one byte, a zero byte, and the number of places that mark
/// (16 bits, little endian, at most 65,535).
pub const PART_TAG: &[u8; 4] = b"VSM1";

/// The program stores slot marks.
pub const MARKS: u8 = 1;
/// A pixel program that writes no depth, stencil reference or coverage,
/// reads no coverage and stores nothing but its outputs and its marks:
/// testing depth and stencil first changes none of its results unless it
/// discards ([`DISCARDS`]) or the pipeline derives coverage from its alpha.
pub const EARLY_OK: u8 = 2;
/// A pixel program that can discard.
pub const DISCARDS: u8 = 4;
/// This translation declares the early depth and stencil test
/// ([`with_early_depth`]).
pub const EARLY: u8 = 8;

/// Which pixel programs mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pixel {
    None,
    Free,
    All,
}

/// The switches, as one translation sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Settings {
    pub on: bool,
    pub pixel: Pixel,
    pub tables: bool,
}

/// [`Settings`] from the switches' values. Pure, for testing.
pub(crate) fn parse(on: Option<&str>, pixel: Option<&str>, tables: Option<&str>) -> Settings {
    Settings {
        on: on.map(str::trim) == Some("1"),
        pixel: match pixel.map(str::trim) {
            Some("none" | "0") => Pixel::None,
            Some("free") => Pixel::Free,
            _ => Pixel::All,
        },
        tables: tables.map(str::trim) == Some("1"),
    }
}

fn from_env() -> Settings {
    static SET: OnceLock<Settings> = OnceLock::new();
    *SET.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        parse(
            var("VICTORY_SLOT_FEEDBACK").as_deref(),
            var("VICTORY_SLOT_FEEDBACK_PIXEL").as_deref(),
            var("VICTORY_SLOT_FEEDBACK_TABLES").as_deref(),
        )
    })
}

thread_local! {
    /// Tests: the settings and the mark slot for translations on this
    /// thread (the process's own stay as they are, so tests translating
    /// beside it are not changed).
    #[cfg(test)]
    pub(crate) static TEST: std::cell::Cell<Option<(Settings, u32)>> = const { std::cell::Cell::new(None) };
    /// [`with_early_depth`]'s choice, for this thread's translations.
    static EARLY_HERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn settings() -> Settings {
    #[cfg(test)]
    if let Some((s, _)) = TEST.with(std::cell::Cell::get) {
        return s;
    }
    from_env()
}

/// Whether programs translated in this process store slot marks
/// (`VICTORY_SLOT_FEEDBACK=1`).
pub fn slot_feedback() -> bool {
    from_env().on
}

/// Which pixel programs do (`VICTORY_SLOT_FEEDBACK_PIXEL`): the marks cover
/// every read only with [`Pixel::All`].
pub fn slot_feedback_pixel() -> Pixel {
    from_env().pixel
}

/// The view heap slot of the buffer the marks go to, if programs translated
/// now store any: the slot below the null slot (`set_view_heap_limit`),
/// which the host keeps out of its own use.
pub fn mark_slot() -> Option<u32> {
    #[cfg(test)]
    if let Some((s, slot)) = TEST.with(std::cell::Cell::get) {
        return s.on.then_some(slot);
    }
    let limit = super::shader::view_heap_limit();
    (from_env().on && limit > 1).then(|| limit - 1)
}

/// Runs `f` (a translation) so that a pixel program with [`MARKS`] and
/// [`EARLY_OK`] declares the early depth and stencil test ([`EARLY`]); any
/// other program comes out as without it. The host asks for this form per
/// pipeline, where the pipeline's state makes it exact. Not part of the
/// translation fingerprint: the caller keeps the two forms apart.
pub fn with_early_depth<R>(on: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            EARLY_HERE.set(self.0);
        }
    }
    let _restore = Restore(EARLY_HERE.replace(on));
    f()
}

pub(crate) fn early_depth() -> bool {
    EARLY_HERE.get()
}

/// What a pixel program with marks at `marks` places does, for [`decide`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// It can discard.
    pub discards: bool,
    /// It stores something besides its outputs and its marks.
    pub writes: bool,
    /// It writes depth, a stencil reference or coverage, or reads coverage.
    pub exports: bool,
}

/// For a pixel program: whether its marks stay (`false`: they are taken
/// out again), and the bits of its `PRIV` part without [`MARKS`].
/// `early`: the early form was asked for.
pub(crate) fn decide(pixel: Pixel, f: Facts, early: bool) -> (bool, u8) {
    let early_ok = !f.exports && !f.writes;
    let keep = match pixel {
        Pixel::All => true,
        Pixel::Free => f.writes || (early_ok && !f.discards),
        Pixel::None => false,
    };
    if !keep {
        return (false, 0);
    }
    let mut bits = 0;
    if early_ok {
        bits |= EARLY_OK;
        if early {
            bits |= EARLY;
        }
    }
    if f.discards {
        bits |= DISCARDS;
    }
    (true, bits)
}

/// The payload of the `PRIV` part of a container whose program marks.
pub(crate) fn part(bits: u8, places: u32) -> [u8; 8] {
    let n = (places.min(0xffff) as u16).to_le_bytes();
    [PART_TAG[0], PART_TAG[1], PART_TAG[2], PART_TAG[3], bits, 0, n[0], n[1]]
}

/// What the translation in the container `dxil` says of its slot marks:
/// the bits ([`MARKS`] and the others) and the number of places that mark;
/// `(0, 0)` for a translation without marks, or anything else.
pub fn slot_marks_of(dxil: &[u8]) -> (u8, u32) {
    let word = |at: usize| dxil.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if dxil.get(..4) != Some(&b"DXBC"[..]) {
        return (0, 0);
    }
    let Some(count) = word(28) else { return (0, 0) };
    for k in 0..count.min(64) {
        let Some(at) = word(32 + 4 * k) else { break };
        let (Some(tag), Some(len)) = (dxil.get(at..at + 4), word(at + 4)) else {
            continue;
        };
        if tag != b"PRIV" || len < 8 {
            continue;
        }
        if let Some(p) = dxil.get(at + 8..at + 16)
            && &p[..4] == PART_TAG
        {
            return (p[4], u32::from(u16::from_le_bytes([p[6], p[7]])));
        }
    }
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_on_only_as_one() {
        assert!(!parse(None, None, None).on);
        assert!(!parse(Some("0"), None, None).on);
        assert!(!parse(Some("alt"), None, None).on);
        let on = parse(Some(" 1 "), None, None);
        assert_eq!(on, Settings { on: true, pixel: Pixel::All, tables: false });
        assert_eq!(parse(Some("1"), Some("free"), Some("1")), Settings { on: true, pixel: Pixel::Free, tables: true });
        assert_eq!(parse(Some("1"), Some("none"), None).pixel, Pixel::None);
        assert_eq!(parse(Some("1"), Some("all"), None).pixel, Pixel::All);
    }

    #[test]
    fn a_pixel_program_keeps_its_marks_as_the_mode_says() {
        let plain = Facts::default();
        let discards = Facts { discards: true, ..plain };
        let writes = Facts { writes: true, ..plain };
        let depth = Facts { exports: true, ..plain };
        // Every program marks; only one without exports and other stores
        // may test early, and says whether it discards.
        assert_eq!(decide(Pixel::All, plain, false), (true, EARLY_OK));
        assert_eq!(decide(Pixel::All, plain, true), (true, EARLY_OK | EARLY));
        assert_eq!(decide(Pixel::All, discards, true), (true, EARLY_OK | EARLY | DISCARDS));
        assert_eq!(decide(Pixel::All, writes, true), (true, 0));
        assert_eq!(decide(Pixel::All, depth, true), (true, 0));
        assert_eq!(decide(Pixel::All, Facts { exports: true, discards: true, ..plain }, true), (true, DISCARDS));
        // Free: only where the mark costs no early test.
        assert_eq!(decide(Pixel::Free, plain, false), (true, EARLY_OK));
        assert_eq!(decide(Pixel::Free, writes, false), (true, 0));
        assert_eq!(decide(Pixel::Free, discards, false), (false, 0));
        assert_eq!(decide(Pixel::Free, depth, false), (false, 0));
        assert_eq!(decide(Pixel::None, plain, true), (false, 0));
    }

    #[test]
    fn the_part_is_found_in_a_container_and_nothing_in_others() {
        let payload = part(MARKS | EARLY_OK, 70_000);
        let dxil = super::super::container::build(&[(b"SFI0", &[0u8; 8][..]), (b"PRIV", &payload[..])]);
        assert_eq!(slot_marks_of(&dxil), (MARKS | EARLY_OK, 65_535));
        let none = super::super::container::build(&[(b"SFI0", &[0u8; 8][..])]);
        assert_eq!(slot_marks_of(&none), (0, 0));
        let other = super::super::container::build(&[(b"PRIV", &b"somebody else's own"[..])]);
        assert_eq!(slot_marks_of(&other), (0, 0));
        assert_eq!(slot_marks_of(b"DXBC"), (0, 0));
        assert_eq!(slot_marks_of(&[]), (0, 0));
    }
}
