//! What `crate::slotguard` and the reader check (`crate::readercheck`)
//! need of the submissions: how many have finished, the lists in flight,
//! and the fault test's early giving back. Apart from `host.rs` for the
//! asset policy's size limit of a source file.

use super::*;

/// Submissions finished so far ([`FINISHED`]): every submission with a
/// number up to this has run on the card.
pub(crate) fn finished() -> u64 {
    FINISHED.load(Ordering::Acquire)
}

/// The reader check's look at the tables of every list in flight or landed
/// and not given back yet (`crate::draw::State::audit`), when the card
/// faulted: the flight that faulted may never land. How many lists.
pub(crate) fn audit_flights() -> usize {
    let line = line();
    let flying = line.line.iter().filter_map(|item| match item {
        Late::Run(flight) => Some(&flight.lists),
        _ => None,
    });
    let landed = line.landed.iter().map(|(lists, _)| lists);
    let mut looked = 0;
    for list in flying.chain(landed).flatten() {
        list.audit("with its list in flight at a fault of the card");
        looked += 1;
    }
    looked
}

/// The submission of `lists` was sent. With the fault test
/// (`VICTORY_SLOT_FAULT_TEST=early`, confirmed) their descriptor tables'
/// slots go back now, while the card still reads them.
pub(super) fn sent(lists: &mut [crate::list::Detached]) {
    if crate::slotguard::fault_test() {
        for list in lists {
            list.tables_back_early();
        }
    }
}
