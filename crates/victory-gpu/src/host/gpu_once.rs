//! `VICTORY_GPU_WRITE_ONCE`: a range shaders write is declared once per list
//! until the title next copies or runs something that writes memory, instead
//! of once per draw that names it. A list of thousands of draws declared the
//! same ranges over and over, and the submission scanned them again each
//! time. Declaring a range again changes nothing in between (idempotent),
//! so the phases of `alt` can change at any time.
//!
//! Ported from the Belobug fork's list-side half of dd799aad (credit in the
//! commit); its submission-side half is not needed here, which indexes the
//! declared ranges by copy already (`step_index`).
//!
//! The base fork now declares each range once per list always (dropping a
//! copy's entries when it is refreshed again); here that is the default, and
//! `VICTORY_GPU_WRITE_ONCE=0` declares per draw as before (`alt`: in turn).

use super::{Deferred, HostList, Step};

static ONCE: crate::alt::Switch = crate::alt::Switch::new_on(
    "VICTORY_GPU_WRITE_ONCE",
    "a range shaders write is declared once per list until the title next writes memory",
);

impl Deferred {
    /// What a title copy or run intervening makes stale.
    pub(super) fn forget_gpu_writes(&mut self) {
        self.gpu_written.clear();
    }
}

impl HostList {
    /// Declares that shaders of the list write `[address, address + len)` of
    /// a buffer's host copy (see `defer_gpu_write`).
    pub(super) fn declare_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        if ONCE.on() {
            let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
            if !deferred.gpu_written.insert(key) {
                return;
            }
        }
        deferred.copies.push(Step::GpuWrite(shadow.clone(), address, len));
    }
}
