//! Large copies between buffers of ours: one `rep movsb` from 64 KiB on
//! (`VICTORY_FAST_COPY=0` turns it off).
//!
//! `std::ptr::copy_nonoverlapping` ends in the C runtime's `memcpy`. Under
//! Wine that is msvcrt's `memmove`, a loop of 16-byte loads and stores,
//! which past the last-level cache moves some 6-8 GB/s where the
//! processor's own string copy (fast on every CPU that reports ERMS) moves
//! 10-11, and from 2 to 4 MiB about twice as much (measured under Wine on
//! a six-core desktop processor; below 1 MiB both are alike).
//!
//! Only for memory of ours that is always there: snapshots, host copies,
//! scratch. Title memory that may be unmapped or guarded under us goes
//! through `crate::host::copy_mapped` / `crate::plaincopy` as before. A
//! guard fault in the middle of the copy would be no different: the
//! instruction resumes where it stopped.

/// The fewest bytes copied with `rep movsb`; below, `memcpy` is as fast.
pub(crate) const LEAST: usize = 64 << 10;

/// Whether large copies use `rep movsb`: on where the processor reports
/// ERMS, unless `VICTORY_FAST_COPY=0`.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(target_arch = "x86_64")
            && erms()
            && std::env::var_os("VICTORY_FAST_COPY").is_none_or(|v| v != "0")
    })
}

#[cfg(target_arch = "x86_64")]
fn erms() -> bool {
    std::arch::is_x86_feature_detected!("ermsb")
}

#[cfg(not(target_arch = "x86_64"))]
fn erms() -> bool {
    false
}

/// Copies `len` bytes from `src` to `dst`, as
/// `std::ptr::copy_nonoverlapping` does.
///
/// # Safety
/// As for `std::ptr::copy_nonoverlapping`: both ranges valid for `len`
/// bytes and not overlapping.
pub(crate) unsafe fn copy(src: *const u8, dst: *mut u8, len: usize) {
    // SAFETY: the caller's.
    unsafe { copy_as(src, dst, len, len >= LEAST && on()) }
}

/// [`copy`], with `rep movsb` or not as `string` says.
///
/// # Safety
/// As for [`copy`].
unsafe fn copy_as(src: *const u8, dst: *mut u8, len: usize, string: bool) {
    #[cfg(target_arch = "x86_64")]
    if string {
        // SAFETY: the caller's ranges; the direction flag is clear at every
        // call by the ABI, so the copy runs upwards.
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rcx") len => _,
                inout("rdi") dst => _,
                inout("rsi") src => _,
                options(nostack, preserves_flags),
            );
        }
        return;
    }
    let _ = string;
    // SAFETY: the caller's.
    unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
}

#[cfg(test)]
mod tests {
    use super::{LEAST, copy, copy_as};

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + i / 253) as u8).collect()
    }

    /// Both ways copy exactly the bytes asked, from any alignment of
    /// either side, and nothing around them.
    #[test]
    fn copies_exactly_the_bytes_asked_at_any_alignment() {
        const PAD: usize = 64;
        for len in [
            0,
            1,
            7,
            63,
            64,
            65,
            4095,
            4097,
            LEAST - 1,
            LEAST,
            LEAST + 1,
            3 * LEAST + 17,
        ] {
            let src = pattern(len + 2 * PAD);
            for string in [false, true] {
                for s in [0, 1, 3, 8, 33] {
                    for d in [0, 1, 5, 16, 63] {
                        let mut dst = vec![0xeeu8; len + 2 * PAD];
                        // SAFETY: inside both buffers; two allocations.
                        unsafe {
                            copy_as(src.as_ptr().add(s), dst.as_mut_ptr().add(d), len, string)
                        };
                        assert!(
                            dst[..d].iter().all(|&b| b == 0xee),
                            "{len} {s} {d} {string}: before"
                        );
                        assert_eq!(dst[d..d + len], src[s..s + len], "{len} {s} {d} {string}");
                        assert!(
                            dst[d + len..].iter().all(|&b| b == 0xee),
                            "{len} {s} {d} {string}: after"
                        );
                    }
                }
            }
        }
    }

    /// The switch's way, large and small.
    #[test]
    fn copy_matches_memcpy() {
        for len in [10, 2 * LEAST + 3, 5 << 20] {
            let src = pattern(len);
            let mut dst = vec![0u8; len];
            // SAFETY: two buffers of `len` bytes.
            unsafe { copy(src.as_ptr(), dst.as_mut_ptr(), len) };
            assert!(dst == src, "{len}");
        }
    }
}
