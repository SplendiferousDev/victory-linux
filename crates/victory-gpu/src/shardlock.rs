//! A read-write lock whose readers each take one of several locks (`VICTORY_VIEWS_SHARDED=1`;
//! `alt`: twenty seconds with, twenty without, in turn, each change said in the log). Off by
//! default: readers take the one lock, as a `std::sync::RwLock` is taken.
//!
//! Why: the table of views (`crate::descriptor`, "de:VIEWS") is read about 195,000 times a
//! frame by the title's five recording threads at once and written rarely (a view not seen
//! before, a released resource). Every read of one `RwLock` adds to and takes from one count,
//! a write to a line the five threads pass between them: in a profile of those threads,
//! 3% of their time was the taking of that lock alone. Here a reader takes the lock of its
//! own shard (by thread), and a writer takes the one lock and every shard's.
//!
//! A writer takes all of them whatever the switch says, so readers of either kind (the one
//! lock, or a shard) are kept out while it writes, also while the switch changes in turn.

use std::cell::UnsafeCell;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Shards: more than the recording threads, each on its own cache line.
const SHARDS: usize = 16;

#[repr(align(128))]
struct Shard(RwLock<()>);

/// See the module.
pub(crate) struct Sharded<T> {
    name: &'static str,
    plain: RwLock<()>,
    shards: [Shard; SHARDS],
    data: UnsafeCell<T>,
}

// SAFETY: the data is reached only through the guards below, which give `&T` under a read
// lock (one of them) and `&mut T` under all of them.
unsafe impl<T: Send + Sync> Sync for Sharded<T> {}

/// Whether readers take a shard now (the switch).
#[cfg(windows)]
fn sharded_now() -> bool {
    static SWITCH: crate::alt::Switch = crate::alt::Switch::new(
        "VICTORY_VIEWS_SHARDED",
        "readers of the table of views take a lock of their own thread's instead of one shared count",
    );
    SWITCH.on()
}

#[cfg(not(windows))]
fn sharded_now() -> bool {
    false
}

/// This thread's shard.
fn shard_index() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static MINE: usize = NEXT.fetch_add(1, Relaxed) % SHARDS;
    }
    MINE.with(|m| *m)
}

/// Shared access: the read lock taken (the one lock's or a shard's), held until dropped.
pub(crate) struct ReadGuard<'a, T> {
    _lock: RwLockReadGuard<'a, ()>,
    data: &'a T,
}

impl<T> std::ops::Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.data
    }
}

pub(crate) struct WriteGuard<'a, T> {
    _plain: RwLockWriteGuard<'a, ()>,
    _shards: [RwLockWriteGuard<'a, ()>; SHARDS],
    data: &'a mut T,
}

impl<T> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.data
    }
}

impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.data
    }
}

impl<T> Sharded<T> {
    pub(crate) const fn new(name: &'static str, data: T) -> Self {
        Sharded {
            name,
            plain: RwLock::new(()),
            shards: [const { Shard(RwLock::new(())) }; SHARDS],
            data: UnsafeCell::new(data),
        }
    }

    /// Shared, through this thread's shard with the switch on, else through the one lock.
    #[track_caller]
    pub(crate) fn read(&self) -> ReadGuard<'_, T> {
        self.read_as(sharded_now())
    }

    #[track_caller]
    fn read_as(&self, sharded: bool) -> ReadGuard<'_, T> {
        if sharded {
            let guard = self.shards[shard_index()].0.read().unwrap_or_else(|e| e.into_inner());
            // SAFETY: a shard's read lock: no writer holds them all.
            return ReadGuard { _lock: guard, data: unsafe { &*self.data.get() } };
        }
        let guard = crate::lockmeter::read(self.name, &self.plain);
        // SAFETY: the one lock read: no writer holds it.
        ReadGuard { _lock: guard, data: unsafe { &*self.data.get() } }
    }

    /// Alone: the one lock, then every shard's in order.
    #[track_caller]
    pub(crate) fn write(&self) -> WriteGuard<'_, T> {
        let plain = crate::lockmeter::write(self.name, &self.plain);
        let shards = std::array::from_fn(|i| self.shards[i].0.write().unwrap_or_else(|e| e.into_inner()));
        WriteGuard {
            _plain: plain,
            _shards: shards,
            // SAFETY: every lock is held: no reader of either kind, no other writer.
            data: unsafe { &mut *self.data.get() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

    #[test]
    fn readers_of_both_kinds_see_what_a_writer_wrote() {
        static L: Sharded<Vec<u32>> = Sharded::new("test", Vec::new());
        L.write().push(1);
        assert_eq!(*L.read_as(false), vec![1]);
        assert_eq!(*L.read_as(true), vec![1]);
        L.write().push(2);
        assert_eq!(L.read_as(true).len(), 2);
    }

    #[test]
    fn a_writer_waits_for_readers_of_either_kind() {
        static L: Sharded<(u64, u64)> = Sharded::new("test", (0, 0));
        static STOP: AtomicBool = AtomicBool::new(false);
        // Writers keep both halves equal; readers of both kinds (in turn) never see them apart.
        let readers: Vec<_> = (0..6)
            .map(|k| {
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    while !STOP.load(SeqCst) {
                        let g = L.read_as((n + k) % 2 == 0);
                        assert_eq!(g.0, g.1, "a read saw a write half done");
                        n += 1;
                    }
                })
            })
            .collect();
        let writers: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..2_000 {
                        let mut g = L.write();
                        g.0 += 1;
                        std::hint::spin_loop();
                        g.1 += 1;
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        STOP.store(true, SeqCst);
        for r in readers {
            r.join().unwrap();
        }
        let g = L.read_as(true);
        assert_eq!((g.0, g.1), (4_000, 4_000));
    }
}
