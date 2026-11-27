//! What the address of a card fault the host's kernel reported
//! (`crate::hostfault`) points at, said once per fault line beside it.
//!
//! NVIDIA's MMU fault lines name the address read (`faulted @
//! 0x5_8d7ff000`, at times with stray bits above the 40 of a GPU address:
//! `0x4005_…`, `0xffffc005_…`). The faults that remain mid-run all fall in
//! two families the same in every run and process: `0x5_8d…` and
//! `0x6_01…`/`0x7_01…`, with a middle field of `0x249`, `0x492` or
//! `0x7ff`. A small program on this PC (NVIDIA under vkd3d-proton with
//! `VK_EXT_descriptor_heap`) takes `0x5_8d249000` reading a constant buffer
//! through a slot that holds a texture view and `0x6_01492000` through one
//! that holds a buffer view: such an address is decoded from the other
//! view's bits (the middle field repeats a 3-bit value four times, as a
//! format's per-component kind would), not from any allocation. A copy of
//! ours that was let go would fault at its own address instead, which the
//! lists of live and let-go copies hold. This line tells the two apart, and
//! carries the slot-kind and reader-check counts at that moment
//! (`crate::gpuwatch::roots_section`).

use std::sync::atomic::{AtomicU32, Ordering};

/// The address after `faulted @ ` in a kernel line, if there is one.
pub(crate) fn address(text: &str) -> Option<u64> {
    let at = text.find("faulted @ ")? + "faulted @ ".len();
    let digits: String = text[at..]
        .trim_start_matches("0x")
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    u64::from_str_radix(&digits, 16).ok()
}

/// The address within the 40 bits a GPU address of ours has.
pub(crate) fn low(address: u64) -> u64 {
    address & ((1 << 40) - 1)
}

/// Which family `address` (its low 40 bits) falls in, if one of those a
/// constant buffer read through a slot of another kind decodes to.
pub(crate) fn family(address: u64) -> Option<&'static str> {
    let a = low(address);
    let field = (a >> 12) & 0xfff;
    if !matches!(field, 0x249 | 0x492 | 0x7ff) {
        return None;
    }
    match a >> 24 {
        0x58d => Some("a texture view read as a constant buffer"),
        0x601 | 0x701 => Some("a buffer view read as a constant buffer"),
        _ => None,
    }
}

/// Lines said so far (at most [`MOST`] a run).
static SAID: AtomicU32 = AtomicU32::new(0);
const MOST: u32 = 8;

/// Says what the address in the fault line `text` points at, if it names
/// one: its family, a live copy of ours holding it, one let go holding it,
/// and the slot-kind counts.
pub(crate) fn say(text: &str) {
    let Some(raw) = address(text) else { return };
    if SAID.fetch_add(1, Ordering::Relaxed) >= MOST {
        return;
    }
    let a = low(raw);
    let family = family(raw).unwrap_or("none known");
    let live = crate::shadow::live_holding(a).unwrap_or_else(|| "none".into());
    let gone = crate::shadow::gone_holding(a, 0).unwrap_or_else(|| "none".into());
    let mut counts = String::new();
    crate::gpuwatch::roots_section(&mut counts);
    tracing::warn!(
        "the card's fault address {raw:#x} ({a:#x} in 40 bits): family {family}; a live copy of ours holding it: {live}; \
         a copy of ours let go lately holding it: {gone}; root views and slot kinds: {counts}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_is_read_with_its_stray_bits() {
        let line = "NVRM: Xid (PCI:0000:01:00): 31, pid=1, name=x, channel 0x00000019, intr 00000000. MMU Fault: ENGINE \
                    GRAPHICS GPC3 GPCCLIENT_GCC faulted @ 0xffffc005_8d7ff000. Fault is of type FAULT_PDE";
        let a = address(line).unwrap();
        assert_eq!(a, 0xffff_c005_8d7f_f000);
        assert_eq!(low(a), 0x5_8d7f_f000);
        assert_eq!(address("NVRM: Xid (PCI:0000:01:00): 13, Graphics SM Warp Exception"), None);
    }

    #[test]
    fn the_families_seen_and_no_others() {
        for a in [0x5_8d24_9000u64, 0x4005_8d49_2000, 0xffff_8005_8d7f_f000] {
            assert_eq!(family(a), Some("a texture view read as a constant buffer"), "{a:#x}");
        }
        for a in [0x6_0149_2000u64, 0x7_0124_9000, 0x4007_017f_f000] {
            assert_eq!(family(a), Some("a buffer view read as a constant buffer"), "{a:#x}");
        }
        for a in [0x5_8d25_0000u64, 0x3_0000_0000, 0x5_5918_0000, 0] {
            assert_eq!(family(a), None, "{a:#x}");
        }
    }
}
