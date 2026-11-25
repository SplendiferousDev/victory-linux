//! The FidelityFX upscale dispatch (`ffxDispatch`).
//!
//! The title's Series S mode renders below its output size and calls the
//! console's FidelityFX upscaler to make the output. The FidelityFX loader
//! and the upscaler are the title's own DLLs and run as they are, but the
//! upscaler's passes are compute pipelines in a container we do not
//! translate: its output stayed black. `ffxDispatch` alone is taken here:
//! an upscale dispatch becomes a plain stretch of the colour input
//! (`victory_gpu::list::upscale_stretch`); anything else, and an upscale we
//! could not stretch, goes to the title's DLL.
//!
//! The import is matched by its symbol name, whichever DLL the title takes
//! it from; the title's own function is what that import resolved to.
//!
//! The structures are the public FidelityFX API's (`ffx_api.h`,
//! `ffx_upscale.h`): a header of a type and a next pointer, and resources of
//! a pointer, a 32-byte description and a state.
//!
//! Hand-ported from ScarlettPort's vendored runtime
//! (`crates/victory-hle/src/modules/ffx.rs`, GPL-3.0-or-later).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// The import taken.
pub const SYMBOL: &str = "ffxDispatch";

/// `VICTORY_UPSCALE_STRETCH=0` leaves the import to the title's DLL.
pub const ENV: &str = "VICTORY_UPSCALE_STRETCH";

static REAL: AtomicUsize = AtomicUsize::new(0);

/// Whether the title's `ffxDispatch` import is taken: on the Series S
/// profile (the one whose output the upscaler makes), unless [`ENV`] is 0.
pub fn wanted(series_s: bool) -> bool {
    series_s && std::env::var_os(ENV).is_none_or(|v| v != "0")
}

/// Takes the import: `real` is the title's own `ffxDispatch`, for what is
/// not stretched here. Returns what the import is bound to instead.
pub fn take(real: usize) -> usize {
    REAL.store(real, Ordering::Relaxed);
    tracing::info!(
        real = format_args!("{real:#x}"),
        "{SYMBOL}: the title's import is ours; upscale dispatches become a plain stretch ({ENV}=0 turns this off)"
    );
    dispatch as *const () as usize
}

#[repr(C)]
struct Header {
    kind: u64,
    next: *const Header,
}

#[repr(C)]
struct Resource {
    resource: *const c_void,
    description: [u32; 8],
    state: u32,
}

#[repr(C)]
struct Upscale {
    header: Header,
    list: *const c_void,
    colour: Resource,
    depth: Resource,
    motion: Resource,
    exposure: Resource,
    reactive: Resource,
    composition: Resource,
    output: Resource,
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    render: [u32; 2],
    upscale: [u32; 2],
}

const _: () = assert!(size_of::<Resource>() == 48);
const _: () = assert!(std::mem::offset_of!(Upscale, output) == 0x138);

/// `FFX_API_RETURN_OK`.
const OK: u32 = 0;
/// `FFX_API_RETURN_ERROR`: what a dispatch nothing could answer returns.
const ERROR: u32 = 1;

/// `FFX_API_DISPATCH_DESC_TYPE_UPSCALE`.
const UPSCALE: u64 = 0x0001_0001;

type Dispatch = extern "system" fn(*mut *mut c_void, *const Header) -> u32;

extern "system" fn dispatch(context: *mut *mut c_void, desc: *const Header) -> u32 {
    // SAFETY: the title passes a dispatch description, whose header says
    // what follows it.
    if let Some(header) = unsafe { desc.as_ref() } {
        if header.kind == UPSCALE {
            // SAFETY: an upscale description, by its type.
            let d = unsafe { &*desc.cast::<Upscale>() };
            if stretched(d) {
                return OK;
            }
        } else {
            static OTHER: AtomicU32 = AtomicU32::new(0);
            if OTHER.fetch_add(1, Ordering::Relaxed) < 4 {
                tracing::info!(
                    kind = format_args!("{:#x}", header.kind),
                    "another FidelityFX dispatch: passed to the title's DLL"
                );
            }
        }
    }
    match REAL.load(Ordering::Relaxed) {
        // Neither stretched nor passed on: the output was not written, and
        // the title is told so instead of presenting it as it is.
        0 => {
            static TOLD: std::sync::Once = std::sync::Once::new();
            TOLD.call_once(|| {
                tracing::error!(
                    "a FidelityFX dispatch was not answered: no stretch was recorded and the title's own {SYMBOL} is not known"
                );
            });
            ERROR
        }
        // SAFETY: the title's own `ffxDispatch`, with its own arguments.
        real => unsafe { std::mem::transmute::<usize, Dispatch>(real)(context, desc) },
    }
}

/// Records the stretch for upscale dispatch `d`; false if it was not, said
/// in the log (the first time, and the first few failures with why).
fn stretched(d: &Upscale) -> bool {
    static DONE: AtomicU32 = AtomicU32::new(0);
    static FAILED: AtomicU32 = AtomicU32::new(0);
    #[cfg(windows)]
    let result = victory_gpu::list::upscale_stretch(d.list, d.colour.resource, d.output.resource, d.render);
    #[cfg(not(windows))]
    let result: Result<(), &str> = Err("no host GPU on this platform");
    match result {
        Ok(()) => {
            if DONE.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::info!(
                    list = ?d.list,
                    colour = ?d.colour.resource,
                    output = ?d.output.resource,
                    render = ?d.render,
                    upscale = ?d.upscale,
                    "upscale dispatch: a stretch of the colour input was recorded in place of the title's upscaler (logged once)"
                );
            }
            true
        }
        Err(why) => {
            let n = FAILED.fetch_add(1, Ordering::Relaxed);
            if n < 4 || n.is_power_of_two() {
                tracing::warn!(
                    why,
                    failures = n + 1,
                    colour = ?d.colour.resource,
                    output = ?d.output.resource,
                    render = ?d.render,
                    "upscale dispatch NOT stretched: passed to the title's upscaler, whose output stays black"
                );
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// The tests that set the title's function run one at a time.
    static REAL_IN_USE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The upscale description as the public API lays it out: what the
    /// stretch reads sits where the title wrote it.
    #[test]
    fn the_upscale_description_is_laid_out_as_the_public_api() {
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(offset_of!(Upscale, list), 0x10);
        assert_eq!(offset_of!(Upscale, colour), 0x18);
        assert_eq!(offset_of!(Upscale, depth), 0x48);
        assert_eq!(offset_of!(Upscale, motion), 0x78);
        assert_eq!(offset_of!(Upscale, output), 0x138);
        assert_eq!(offset_of!(Upscale, jitter), 0x168);
        assert_eq!(offset_of!(Upscale, motion_scale), 0x170);
        assert_eq!(offset_of!(Upscale, render), 0x178);
        assert_eq!(offset_of!(Upscale, upscale), 0x180);
        assert_eq!(offset_of!(Resource, description), 8);
        assert_eq!(offset_of!(Resource, state), 40);
    }

    /// A dispatch that is not an upscale, with no DLL of the title's to
    /// pass it to, is an error and not a silent success; so is an upscale
    /// that names nothing to stretch.
    #[test]
    fn a_dispatch_nothing_answers_is_an_error() {
        let _one = REAL_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        let before = REAL.swap(0, Ordering::Relaxed);
        let other = Header {
            kind: 0x0002_0001,
            next: std::ptr::null(),
        };
        assert_eq!(dispatch(std::ptr::null_mut(), &other), ERROR);
        assert_eq!(dispatch(std::ptr::null_mut(), std::ptr::null()), ERROR);
        // SAFETY: all-zero is a valid description (null pointers, zero sizes).
        let mut empty: Upscale = unsafe { std::mem::zeroed() };
        empty.header.kind = UPSCALE;
        assert_eq!(dispatch(std::ptr::null_mut(), &empty.header), ERROR);
        REAL.store(before, Ordering::Relaxed);
    }

    /// What is not stretched goes to the title's own function, with its
    /// own arguments and result.
    #[test]
    fn what_is_not_stretched_goes_to_the_titles_dispatch() {
        extern "system" fn title(_: *mut *mut c_void, desc: *const Header) -> u32 {
            // SAFETY: the test passes a header.
            0x40 + unsafe { (*desc).kind } as u32
        }
        let _one = REAL_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        let ours = take(title as *const () as usize);
        assert_eq!(ours, dispatch as *const () as usize);
        let other = Header {
            kind: 2,
            next: std::ptr::null(),
        };
        assert_eq!(dispatch(std::ptr::null_mut(), &other), 0x42);
        REAL.store(0, Ordering::Relaxed);
    }

    /// Only the Series S profile takes the import.
    #[test]
    fn only_series_s_takes_the_import() {
        assert!(!wanted(false));
    }
}
