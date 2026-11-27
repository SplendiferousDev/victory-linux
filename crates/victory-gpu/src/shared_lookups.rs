//! Asks for a buffer's host copy under a shared lock (`VICTORY_SHARED_LOOKUPS=1`; `alt`: in
//! turn; off unless set).
//!
//! Every draw's and every view's ask for the host copy of the memory it reads
//! (`crate::shadow::find`) took the one lock of the table of title buffers, and the window
//! table's after it, for writing; the title's recording threads (five or six at once) and the
//! pass over the view heap (tens of thousands of asks in a row on one of them) then queue on
//! it, each wait a trip into the kernel under Wine. With the switch an ask that finds a copy
//! already made takes both tables' locks for reading only; an ask that has to make one, and
//! everything that changes the tables, takes them for writing as before.
//!
//! What it changes: nothing an ask finds (the same search over the same table, the same
//! notes on a window handed out, which are atomics). Readers still exclude every writer, so
//! the orderings the tables' changes rely on (`TABLES_CHANGED`, the idle sweep) hold.

static SWITCH: crate::alt::Switch = crate::alt::Switch::new(
    "VICTORY_SHARED_LOOKUPS",
    "asks for a buffer's host copy that find one take the tables' locks for reading only",
);

/// Whether asks that find a copy take the tables' locks for reading.
pub(crate) fn on() -> bool {
    SWITCH.on()
}
