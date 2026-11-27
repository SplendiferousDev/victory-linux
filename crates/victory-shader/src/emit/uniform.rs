//! Which VGPR values hold the same thing in every lane of a console wave: the
//! analysis behind first-lane reads on hosts with narrower waves. A child of
//! the emitter's `lower` module, split out to keep that file under the size limit.

use super::*;

/// VGPR values that are the same in every lane of a console wave holding them (`all` is
/// [`all_lane_blocks`]): written lane by lane from scalars, constants and such values, by an
/// instruction that either every arriving lane runs or that runs under a condition itself the
/// same in every lane of the console wave (then only over an old value that is such a value
/// too: a wave the condition leaves out keeps that); loaded from LDS at such an address; or
/// the thread id's x shifted right past the console wave (`wave_tid`, ids `>> 6` or more,
/// where a console wave is one run of 64 ids along x). The compiler keeps wave-uniform data
/// in VGPRs and fetches it with `v_readfirstlane`.
/// A value joined by a phi counts where every predecessor is a block every lane of the console
/// wave reaches together: no branch on a lane-dependent condition can have split them (a
/// branch on such alike values does not). Loop-carried values count only when what the loop
/// carries is made of such values; the analysis runs to a fixed point from nothing, so a value
/// that needs itself to be alike never is.
pub(super) fn lane_uniform(
    p: &Program,
    st: &Structured,
    all: &[bool],
    wave_tid: Option<Value>,
) -> HashSet<Value> {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let mut same: HashSet<Value> = HashSet::new();
    loop {
        // An atom of a lane condition that every lane of a console wave holds alike.
        let alike_atom = |same: &HashSet<Value>, a: &Atom| match *a {
            Atom::Uniform(_) => true,
            Atom::Lane(v) => same.contains(&v),
            _ => false,
        };
        let alike_bool = |same: &HashSet<Value>, b: Bool| {
            st.bools.support(b).iter().all(|a| alike_atom(same, a))
        };
        // Blocks every lane of a console wave reaches together: `all`, or reached only past
        // branches on alike conditions.
        let wave_all: Vec<bool> = match lane_regions(st, false) {
            Some(regions) => {
                let mut w = vec![true; st.exits.len()];
                for (d, region) in regions {
                    let LaneExit::Branch { when, .. } = st.exits[d] else {
                        continue;
                    };
                    if alike_bool(&same, when) {
                        continue;
                    }
                    for (b, r) in region.iter().enumerate() {
                        if *r {
                            w[b] = false;
                        }
                    }
                }
                w
            }
            None => all.to_vec(),
        };
        let mut next: HashSet<Value> = HashSet::new();
        for b in crate::ir::reverse_postorder(&p.blocks) {
            let block = &p.blocks[b];
            let whole = block
                .preds
                .iter()
                .all(|&q| wave_all.get(q).copied().unwrap_or(false));
            for phi in &block.phis {
                if matches!(phi.reg, Reg::V(_))
                    && whole
                    && phi
                        .args
                        .iter()
                        .all(|a| next.contains(a) || same.contains(a))
                {
                    next.insert(phi.value);
                }
            }
            let known = |next: &HashSet<Value>, x: &Value| next.contains(x) || same.contains(x);
            for (i, inst) in block.insts.iter().enumerate() {
                let name = inst.name();
                // under a condition: only one every lane of the console wave holds alike, and
                // over an old value that is alike too
                if let Some(g) = st.guards[b][i] {
                    if !alike_bool(&same, g) {
                        continue;
                    }
                    let old_alike = inst
                        .writes
                        .iter()
                        .filter(|w| matches!(w.0, Reg::V(_)))
                        .all(|w| inst.read(w.0).is_none_or(|old| known(&next, &old)));
                    if !old_alike {
                        continue;
                    }
                }
                // an LDS read at an alike address reads one value in every lane
                // (`_d16` forms keep the other half of the destination register and are left
                // out; base fork 1320dfd7.)
                if let Fields::Ds {
                    addr: Some(Operand::Vector { reg, .. }),
                    dst: Some(_),
                    data,
                    gds: false,
                    ..
                } = &inst.fields
                    && name.starts_with("ds_read")
                    && !name.contains("_d16")
                    && data.is_empty()
                {
                    if inst.read(Reg::V(*reg)).is_some_and(|a| known(&next, &a)) {
                        next.extend(
                            inst.writes
                                .iter()
                                .filter(|w| matches!(w.0, Reg::V(_)))
                                .map(|w| w.1),
                        );
                    }
                    continue;
                }
                let ops: Vec<Operand> = match &inst.fields {
                    Fields::Valu(v)
                        if v.dpp.is_none()
                            && v.sdwa.is_none()
                            && !name.starts_with("v_mbcnt")
                            && !name.starts_with("v_cmpx")
                            && !name.contains("lane") =>
                    {
                        v.src.clone()
                    }
                    _ => continue,
                };
                // the thread id's x shifted past the console wave: one value per console wave
                // (an inline constant 6..64: operand codes 134..192)
                if let (
                    Some(tid),
                    "v_lshrrev_b32",
                    [Operand::Const(k), Operand::Vector { reg, .. }, ..],
                ) = (wave_tid, name, ops.as_slice())
                    && (134..=192).contains(k)
                    && inst.read(Reg::V(*reg)) == Some(tid)
                {
                    next.extend(inst.writes.iter().filter(|w| w.0 != Reg::Scc).map(|w| w.1));
                    continue;
                }
                let alike = ops.iter().all(|op| match *op {
                    Operand::Vector { reg, dwords } => {
                        // every dword of the operand the instruction reads (not the old value of a
                        // higher-numbered destination register that IR keeps for guards). A
                        // relative move reads the register `M0` past this one, and a swap keeps
                        // or moves old values: for those every register read from this one on
                        // counts. (Base fork 1320dfd7.)
                        let whole = name.starts_with("v_movrel") || name.starts_with("v_swap");
                        let end = reg.saturating_add(dwords);
                        let read: Vec<Value> = inst
                            .reads
                            .iter()
                            .filter(|r| matches!(r.0, Reg::V(x) if x >= reg && (whole || x < end)))
                            .map(|r| r.1)
                            .collect();
                        !read.is_empty() && read.iter().all(|x| known(&next, x))
                    }
                    // a scalar is one value; a lane mask is not, unless a compare of such values made it
                    Operand::Scalar { code, dwords } if code != NULL => (code..code.saturating_add(dwords))
                        .all(|c| inst.read(Reg::S(c)).is_none_or(|x| !is_mask(x) || known(&next, &x))),
                    _ => true,
                });
                if alike {
                    next.extend(inst.writes.iter().filter(|w| w.0 != Reg::Scc).map(|w| w.1));
                }
            }
        }
        if next.is_subset(&same) {
            return same;
        }
        same.extend(next);
    }
}

/// The thread id's x (`v0` on entry) of a console wave64 compute program whose console waves are
/// runs of 64 ids along x (a group 64-aligned along x, or one row): `id >> 6` and up is then one
/// value per console wave ([`lane_uniform`]).
pub(super) fn wave_tid(p: &Program, x: &Parts<'_>) -> Option<Value> {
    let StageIo::Cs(cs) = &x.native.io else {
        return None;
    };
    let [tx, ty, tz] = cs.threads;
    if !x.native.wave64 || !(tx % 64 == 0 || ty * tz == 1) {
        return None;
    }
    p.inputs
        .iter()
        .find(|(r, _)| *r == Reg::V(0))
        .map(|(_, v)| *v)
}

/// Whether `v` is the same in every lane of its console wave in the
/// surrounding control flow: constants, scalar reads, and lane operations
/// whose inputs all are, through loop-carried phis. An LDS read at a
/// uniform address counts only while no later LDS write can intervene:
/// every lane asks for the same dword, but both halves must observe the
/// same write (the shader uses this for a wave's running scan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Uniform {
    Yes,
    No,
    /// Only reachable through the value being checked (a loop back edge).
    Cycle,
}

pub(super) fn lds_stable(p: &Program, at: usize) -> bool {
    p.blocks.iter().skip(at).all(|b| {
        b.insts
            .iter()
            .all(|i| !i.name().starts_with("ds_") || i.name().starts_with("ds_read"))
    })
}

fn operand_uniform(
    p: &Program,
    st: &Structured,
    at: usize,
    inst: &Inst,
    op: &Operand,
    path: &mut HashSet<Value>,
) -> Uniform {
    match *op {
        Operand::Vector { reg, .. } => inst
            .read(Reg::V(reg))
            .map_or(Uniform::No, |x| value_uniform(p, st, at, x, path)),
        Operand::Scalar { code, .. } => inst
            .read(Reg::S(code))
            .filter(|x| st.masks.contains_key(x) || st.highs.contains_key(x))
            .map_or(Uniform::Yes, |_| Uniform::No),
        _ => Uniform::Yes,
    }
}

pub(super) fn value_uniform(
    p: &Program,
    st: &Structured,
    at: usize,
    v: Value,
    path: &mut HashSet<Value>,
) -> Uniform {
    if !path.insert(v) {
        return Uniform::Cycle;
    }
    let mut result = Uniform::No;
    'find: {
        for block in &p.blocks {
            for phi in &block.phis {
                if phi.value != v {
                    continue;
                }
                let (mut yes, mut cycle) = (false, false);
                for &a in &phi.args {
                    match value_uniform(p, st, at, a, path) {
                        Uniform::No => {
                            result = Uniform::No;
                            break 'find;
                        }
                        Uniform::Yes => yes = true,
                        Uniform::Cycle => cycle = true,
                    }
                }
                // A loop's back edge alone is not a seed; at least one
                // incoming value must come from outside the cycle.
                result = if yes {
                    Uniform::Yes
                } else if cycle {
                    Uniform::Cycle
                } else {
                    Uniform::Yes
                };
                break 'find;
            }
            for inst in &block.insts {
                if !inst.writes.iter().any(|&(_, w)| w == v) {
                    continue;
                }
                let name = inst.name();
                if name.starts_with("ds_read") {
                    if !lds_stable(p, at) {
                        result = Uniform::No;
                        break 'find;
                    }
                    let Fields::Ds { addr, .. } = &inst.fields else {
                        result = Uniform::No;
                        break 'find;
                    };
                    result = addr
                        .as_ref()
                        .map_or(Uniform::Yes, |a| operand_uniform(p, st, at, inst, a, path));
                    break 'find;
                }
                if name.starts_with("v_mbcnt")
                    || name.contains("lane")
                    || name.starts_with("v_cmpx")
                {
                    result = Uniform::No;
                    break 'find;
                }
                let Fields::Valu(val) = &inst.fields else {
                    result = Uniform::No;
                    break 'find;
                };
                if val.dpp.is_some() || val.sdwa.is_some() {
                    result = Uniform::No;
                    break 'find;
                }
                let mut yes = false;
                let mut cycle = false;
                for op in &val.src {
                    match operand_uniform(p, st, at, inst, op, path) {
                        Uniform::No => {
                            result = Uniform::No;
                            break 'find;
                        }
                        Uniform::Yes => yes = true,
                        Uniform::Cycle => cycle = true,
                    }
                }
                result = if yes {
                    Uniform::Yes
                } else if cycle {
                    Uniform::Cycle
                } else {
                    Uniform::Yes
                };
                break 'find;
            }
        }
    }
    path.remove(&v);
    result
}
