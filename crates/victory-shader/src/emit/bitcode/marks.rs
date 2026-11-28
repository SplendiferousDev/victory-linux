//! What a module does besides its outputs and its slot marks
//! (`emit::slot_mark`), and taking the marks out again.

use super::opt::{Effect, effect};
use super::{Module, Op, V};

/// What the calls reached from the entry do: see [`Module::sides`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sides {
    /// A discard.
    pub discards: bool,
    /// A store, an atomic or a barrier that is neither an output nor a
    /// slot mark.
    pub writes: bool,
    /// Slot marks: raw stores through the marks' handle.
    pub marks: u32,
}

impl Module {
    /// Whether instruction `i` is a raw store through the handle `mark`.
    fn is_mark(&self, i: u32, mark: V) -> bool {
        match &self.insts[i as usize].op {
            Op::Call(f, args) => {
                self.funcs[*f as usize].name.starts_with("dx.op.rawBufferStore") && args.get(1) == Some(&mark)
            }
            _ => false,
        }
    }

    /// The effects of the calls reached from the entry, the stores through
    /// `mark` counted apart.
    pub(crate) fn sides(&self, mark: Option<V>) -> Sides {
        let mut s = Sides::default();
        let reached = self.reached();
        for (_, blk) in self.blocks.iter().enumerate().filter(|(b, _)| reached[*b]) {
            for &i in &blk.body {
                let Op::Call(f, _) = &self.insts[i as usize].op else {
                    continue;
                };
                let func = &self.funcs[*f as usize];
                if effect(func) != Effect::Writes {
                    continue;
                }
                let name = func.name.strip_prefix("dx.op.").unwrap_or(&func.name);
                if mark.is_some_and(|m| self.is_mark(i, m)) {
                    s.marks += 1;
                } else if name == "discard" {
                    s.discards = true;
                } else if !name.starts_with("storeOutput") {
                    s.writes = true;
                }
            }
        }
        s
    }

    /// Takes the stores through `mark` out of every block (the handle is
    /// left to the clean-up). Returns how many.
    pub(crate) fn drop_marks(&mut self, mark: V) -> u32 {
        let mut gone = 0;
        for b in 0..self.blocks.len() {
            let body = std::mem::take(&mut self.blocks[b].body);
            let before = body.len();
            let kept: Vec<u32> = body.into_iter().filter(|&i| !self.is_mark(i, mark)).collect();
            gone += (before - kept.len()) as u32;
            self.blocks[b].body = kept;
        }
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Attr, Term};
    use super::*;

    /// A module of one block: an output, `discard` if asked, a store through
    /// `other`, and two through `mark`.
    fn module(discard: bool, other: bool) -> (Module, V) {
        let mut m = Module::default();
        let (void, i32t, i1) = (m.void(), m.int(32), m.int(1));
        let main_ty = m.func_ty(void, vec![]);
        m.ptr(main_ty);
        m.function("main", main_ty, None, true);
        let b = m.new_block();
        let fty = m.func_ty(void, vec![i32t, i32t, i32t]);
        m.ptr(fty);
        let store = m.function("dx.op.rawBufferStore.i32", fty, Some(Attr::NoUnwind), false);
        let out = m.function("dx.op.storeOutput.i32", fty, Some(Attr::NoUnwind), false);
        let dty = m.func_ty(void, vec![i32t, i1]);
        m.ptr(dty);
        let kill = m.function("dx.op.discard", dty, Some(Attr::NoUnwind), false);
        let (mark, elsewhere, k) = (m.i32(7), m.i32(8), m.i32(140));
        let one = m.i32(1);
        let k5 = m.i32(5);
        m.call(b, out, vec![k5, one, one]);
        if discard {
            let t = m.bool(true);
            let k82 = m.i32(82);
            m.call(b, kill, vec![k82, t]);
        }
        if other {
            m.call(b, store, vec![k, elsewhere, one]);
        }
        m.call(b, store, vec![k, mark, one]);
        m.call(b, store, vec![k, mark, one]);
        m.terminate(b, Term::Ret);
        (m, mark)
    }

    #[test]
    fn outputs_and_marks_are_no_other_effect() {
        let (m, mark) = module(false, false);
        assert_eq!(m.sides(Some(mark)), Sides { discards: false, writes: false, marks: 2 });
        // Without the handle named, its stores are stores like any other.
        assert_eq!(m.sides(None), Sides { discards: false, writes: true, marks: 0 });
        let (m, mark) = module(true, true);
        assert_eq!(m.sides(Some(mark)), Sides { discards: true, writes: true, marks: 2 });
    }

    #[test]
    fn marks_taken_out_leave_the_rest() {
        let (mut m, mark) = module(true, true);
        assert_eq!(m.drop_marks(mark), 2);
        assert_eq!(m.drop_marks(mark), 0);
        assert_eq!(m.sides(Some(mark)), Sides { discards: true, writes: true, marks: 0 });
    }
}
