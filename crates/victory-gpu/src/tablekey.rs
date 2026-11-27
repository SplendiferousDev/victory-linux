//! The key of a table made afresh in per-draw slots
//! (`crate::draw::State::tables`), by which a list finds a table it made
//! before.
//!
//! Such a table depends on more than its records: the entries the shader
//! moves halves through are made in another format, and the entries the
//! shader reads as constant buffers are made constant buffer views
//! whatever the title's records hold there (a texture view, a buffer view,
//! nothing: `crate::slotkind`). A table made for one stage, reused by
//! another that reads one more entry as a constant buffer, would show it
//! that entry as the title's view: NVIDIA's constant cache then reads an
//! address decoded from that view's bits and faults (Xid 31). So the
//! entries read as constant buffers that hold no constant buffer view
//! ([`forced`]) are part of the key, as the halves are
//! (`VICTORY_TABLE_KEY_CBV`, on unless `0`; `alt` in turn).

use victory_shader::{ResourceType, UsedResource};

use crate::descriptor::View;

static KEY_CBV: crate::alt::Switch = crate::alt::Switch::new_on(
    "VICTORY_TABLE_KEY_CBV",
    "a table made afresh is found again only by a stage that reads the same entries as constant buffers",
);

/// Whether the entries read as constant buffers are in the key now.
pub(crate) fn key_cbv() -> bool {
    KEY_CBV.on()
}

fn mix(h: u64, x: u64) -> u64 {
    (h.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// The key of a table of `base` (its address and records' hash), with the
/// slots `halves` made in other formats and the entries `forced` made
/// constant buffer views over what they hold. With no `forced` it is the
/// key as it was before them.
pub(crate) fn key(base: u64, halves: &[u32], forced: &[u32]) -> u64 {
    let h = halves.iter().fold(base, |h, &k| mix(h, u64::from(k + 1)));
    // (Apart from the halves: the same slot in both says something else.)
    forced.iter().fold(h, |h, &k| mix(h, u64::from(k + 1) | 1 << 40))
}

/// The entries among `entries` (one stage's of one table of `n` slots)
/// read as constant buffers whose view `view(k)` is not a constant buffer
/// view: those the table makes constant buffer views for this stage alone.
/// In order, once each.
pub(crate) fn forced(entries: &[&UsedResource], n: u32, view: impl Fn(u32) -> Option<View>) -> Vec<u32> {
    let mut out: Vec<u32> = entries
        .iter()
        .filter(|u| u.kind == ResourceType::Cbv)
        .map(|u| u.table_offset_dwords / 8)
        .filter(|&k| k < n && !matches!(view(k), Some(View::Constant(..))))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::RootType;

    fn entry(kind: ResourceType, k: u32) -> UsedResource {
        UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: 8 * k,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }
    }

    /// Synthetic table of three slots: 0 a constant buffer view, 1 a
    /// texture view, 2 nothing.
    fn view(k: u32) -> Option<View> {
        match k {
            0 => Some(View::Constant(0x1000, 256)),
            1 => Some(View::Shader(0x2000, None)),
            _ => None,
        }
    }

    /// Stage A reads entry 0 as constants and entry 1 as a texture; stage B
    /// reads entries 0 and 1 as constants. Same table, same records, same
    /// halves: B must not find A's table (entry 1 there is the texture
    /// view, which B would read as a constant buffer).
    #[test]
    fn a_stage_reading_another_entry_as_constants_gets_its_own_table() {
        let (a0, a1) = (entry(ResourceType::Cbv, 0), entry(ResourceType::Srv, 1));
        let (b0, b1) = (entry(ResourceType::Cbv, 0), entry(ResourceType::Cbv, 1));
        let a = forced(&[&a0, &a1], 3, view);
        let b = forced(&[&b0, &b1], 3, view);
        assert_eq!(a, Vec::<u32>::new(), "entry 0 holds a constant view already");
        assert_eq!(b, vec![1]);
        let base = 0x1234_5678_9abc_def0;
        assert_ne!(key(base, &[], &a), key(base, &[], &b));
        assert_ne!(a, b, "the comparison after the hash tells them apart too");
    }

    #[test]
    fn stages_differing_only_in_entries_that_hold_constant_views_share() {
        let (a0, b0, b2) = (entry(ResourceType::Srv, 0), entry(ResourceType::Cbv, 0), entry(ResourceType::Cbv, 2));
        assert_eq!(forced(&[&a0], 3, view), forced(&[&b0], 3, view));
        // An entry past the table's slots is not made, and not keyed.
        assert_eq!(forced(&[&b2], 2, view), Vec::<u32>::new());
        assert_eq!(forced(&[&b2, &b2], 3, view), vec![2]);
    }

    #[test]
    fn without_forced_entries_the_key_is_the_old_one() {
        let base: u64 = 77;
        let old = [3u32, 5].iter().fold(base, |h, &k| {
            (h.rotate_left(5) ^ u64::from(k + 1)).wrapping_mul(0x517c_c1b7_2722_0a95)
        });
        assert_eq!(key(base, &[3, 5], &[]), old);
        assert_ne!(key(base, &[3], &[]), key(base, &[], &[3]), "halves and constants apart");
    }
}
