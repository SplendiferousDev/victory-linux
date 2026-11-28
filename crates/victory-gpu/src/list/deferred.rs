//! A list's side of `crate::thin` (`VICTORY_THIN=1`): the calls that are
//! written down, what is copied with them, and how a thread of ours makes
//! them.
//!
//! Written down: the draw state setters, the pipeline, draws, dispatches,
//! mesh dispatches, indirect executions, barriers and the close. Every other list
//! call finds the stream empty first ([`drain`], through
//! `super::host_list`) and goes on as before.
//!
//! What a call names is looked at later than before, by the thread that
//! translates it and before the list is submitted:
//!
//! - What D3D12 reads at the call is copied at the call: root constants,
//!   viewports, scissors, the blend factor, vertex and index buffer views,
//!   the heaps' names, and the records of the render target and depth views
//!   (the title writes those slots again right after).
//! - Descriptor tables, root views and indirect arguments are addresses.
//!   What they hold is the card's to read when the list runs, on the
//!   console as here, so the title leaves it alone until then; our tables'
//!   records are read at the translation, the bytes at the submission as
//!   before. `VICTORY_THIN_CHECK=1` counts tables that began otherwise at
//!   the translation than at the call.
//! - Objects the call names are held (a reference of ours) until the list
//!   is submitted or reset: the title may release a pipeline before it
//!   submits the list. What only a descriptor names is kept by
//!   `crate::thin::before_free`.
//!
//! The design is Maren Halvorsen's (VIctory-Fast, `list/thin.rs`, dd83629,
//! b1c7f60), written anew here.

use std::sync::Arc;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT;

use super::{List, This};
use crate::draw::{IndexView, VertexView};
use crate::thin::{Stream, Why, items, put_items};

/// The words of a render target view's record.
const RECORD: usize = size_of::<crate::descriptor::RtvRecord>().div_ceil(8);
/// Root parameters whose tables [`Front::mark`] looks at.
const SLOTS: usize = 32;

/// A list call written down. What it names beyond its own words is in the
/// stream's words (`at`, `count`).
#[derive(Clone, Copy)]
pub(super) enum Call {
    Topology(u32),
    Viewports { at: u32, count: u32 },
    Scissors { at: u32, count: u32 },
    Blend(Option<[f32; 4]>),
    StencilRef(u32),
    DepthBounds(f32, f32),
    Heaps { at: u32, count: u32 },
    Table { compute: bool, slot: u32, handle: u64 },
    Constant { compute: bool, slot: u32, value: u32, offset: u32 },
    Constants { compute: bool, slot: u32, offset: u32, at: u32, count: u32 },
    View { compute: bool, slot: u32, address: u64 },
    Index(Option<IndexView>),
    Vertex { start: u32, count: u32, at: u32, given: bool },
    /// The records are in memory the list's [`Front`] keeps until its
    /// reset: nine of [`RECORD`] words, the depth view's last.
    Targets { records: usize, count: u32, present: u32, depth: bool },
    Pipeline(usize),
    Draw { vertices: u32, instances: u32, first: u32, first_instance: u32 },
    DrawIndexed { indices: u32, instances: u32, first: u32, base: i32, first_instance: u32 },
    Dispatch { x: u32, y: u32, z: u32 },
    DispatchX { x: u32, y: u32, z: u32, flags: u32 },
    DispatchMesh { x: u32 },
    Indirect { signature: usize, max: u32, arguments: usize, offset: u64, count: usize, count_offset: u64 },
    /// `count` of the list's barriers in the words.
    Barriers { at: u32, count: u32 },
    BarrierGroups,
    /// What the tables at the handles in the words began with at the call
    /// (`VICTORY_THIN_CHECK=1`).
    Mark { at: u32, count: u32, print: u64 },
    Close,
}

/// What a title list keeps for its calls written down. Only the thread
/// that records into the list (or submits, resets or releases it) touches
/// it.
#[derive(Default)]
pub(super) struct Front {
    stream: Option<Arc<Stream<Call>>>,
    /// Objects the calls name, each with a reference of ours, and whether
    /// views may name it too (a resource; not a pipeline, a signature or a
    /// heap).
    held: Vec<(usize, bool)>,
    /// The view records of the render targets set ([`Call::Targets`]).
    targets: Vec<Box<[u64]>>,
    /// For [`Front::mark`]: the heaps and tables the title set, graphics
    /// then compute.
    heaps: [usize; 2],
    tables: [[u64; SLOTS]; 2],
}

impl Drop for Front {
    fn drop(&mut self) {
        self.settle();
    }
}

/// The front of list `this` if its calls are written down: not on a thread
/// of ours that translates, which makes them.
#[inline]
pub(super) fn front<'a>(this: This) -> Option<&'a mut Front> {
    if !crate::thin::on() || this.is_null() || crate::thin::translating() {
        return None;
    }
    // SAFETY: the title passes one of our lists; one thread records into a
    // list at a time, and the threads that translate never touch this field.
    Some(unsafe { &mut (*this.cast_mut().cast::<List>()).thin })
}

/// Waits until the calls written down for `this` are made: a call that is
/// not written down follows.
pub(super) fn drain(this: This) {
    if let Some(front) = front(this)
        && let Some(stream) = &front.stream
    {
        stream.drain(Why::Call);
    }
}

/// As [`drain`], at the list's submission or reset: what was held for its
/// calls is let go too.
pub(super) fn settle(this: This) {
    if let Some(front) = front(this) {
        front.settle();
    }
}

fn failed(what: &str) {
    crate::host::fail(format_args!("{what}"))
}

/// What the tables at `handles` begin with.
fn print(handles: &[u64]) -> u64 {
    handles.iter().fold(0x9e37_79b9_7f4a_7c15, |h, &handle| {
        // SAFETY: a slot of a descriptor heap the list has set (`mark`).
        let [a, b, c] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let h = (h.rotate_left(5) ^ (u64::from(a) | u64::from(b) << 32)).wrapping_mul(0x517c_c1b7_2722_0a95);
        (h.rotate_left(5) ^ u64::from(c)).wrapping_mul(0x517c_c1b7_2722_0a95)
    })
}

impl Front {
    fn stream(&mut self, this: This) -> &Arc<Stream<Call>> {
        self.stream.get_or_insert_with(|| Stream::new(this as usize, apply, failed))
    }

    pub(super) fn put(&mut self, this: This, call: Call) {
        self.stream(this).put(|_| call);
    }

    /// Keeps `object` alive until the list is submitted or reset.
    pub(super) fn hold(&mut self, object: This, viewed: bool) {
        if object.is_null() || self.held.iter().rev().take(8).any(|&(h, _)| h == object as usize) {
            return;
        }
        crate::object::add_ref(object);
        self.held.push((object as usize, viewed));
    }

    pub(super) fn settle(&mut self) {
        if let Some(stream) = &self.stream {
            stream.drain(Why::End);
        }
        for (object, viewed) in self.held.drain(..) {
            if viewed {
                crate::object::release_by_table(object as This);
            } else {
                crate::thin::unnamed(|| crate::object::release_by_table(object as This));
            }
        }
        self.targets.clear();
        self.heaps = [0; 2];
        self.tables = [[0; SLOTS]; 2];
    }

    pub(super) fn close(&mut self, this: This) {
        self.put(this, Call::Close);
        // The title's next call on this list is its submission, some time
        // later: a thread of ours gets on with what is left meanwhile.
        if let Some(stream) = &self.stream {
            stream.flush();
        }
    }

    pub(super) fn put_all<T: Copy>(&mut self, this: This, items: &[T], call: impl FnOnce(u32, u32) -> Call) {
        self.stream(this).put(|words| call(put_items(words, items), items.len() as u32));
    }

    pub(super) fn heaps(&mut self, this: This, heaps: &[This]) {
        if crate::thin::check() {
            self.heaps = [0; 2];
        }
        for &heap in heaps {
            self.hold(heap, false);
            if crate::thin::check() && !heap.is_null() {
                self.heaps[usize::from(crate::descriptor::heap(heap).samplers())] = heap as usize;
            }
        }
        self.put_all(this, heaps, |at, count| Call::Heaps { at, count });
    }

    pub(super) fn table(&mut self, this: This, compute: bool, slot: u32, handle: u64) {
        if crate::thin::check()
            && let Some(table) = self.tables[usize::from(compute)].get_mut(slot as usize)
        {
            *table = handle;
        }
        self.put(this, Call::Table { compute, slot, handle });
    }

    pub(super) fn barriers(&mut self, this: This, barriers: &[super::Barrier]) {
        for b in barriers {
            self.hold(b.first, true);
            // (An aliasing barrier's second resource.)
            if b.kind == 1 {
                self.hold(b.second as usize as This, true);
            }
        }
        self.put_all(this, barriers, |at, count| Call::Barriers { at, count });
    }

    pub(super) fn pipeline(&mut self, this: This, pipeline: This) {
        self.hold(pipeline, false);
        self.put(this, Call::Pipeline(pipeline as usize));
    }

    pub(super) fn targets(&mut self, this: This, handles: &[u64], depth: u64) {
        let count = handles.len().min(8);
        let mut records = vec![0u64; RECORD * 9].into_boxed_slice();
        let mut copy = |k: usize, handle: u64| {
            // SAFETY: a view's slot the title passes (they are 128 bytes),
            // into the words of record `k`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    handle as *const u8,
                    records[k * RECORD..].as_mut_ptr().cast::<u8>(),
                    size_of::<crate::descriptor::RtvRecord>(),
                )
            };
        };
        let mut present = 0;
        for (k, &handle) in handles.iter().take(8).enumerate() {
            if handle != 0 {
                copy(k, handle);
                present |= 1 << k;
            }
        }
        if depth != 0 {
            copy(8, depth);
        }
        let at = records.as_ptr() as usize;
        self.targets.push(records);
        self.put(this, Call::Targets { records: at, count: count as u32, present, depth: depth != 0 });
    }

    #[allow(clippy::too_many_arguments)] // the method's own arguments
    pub(super) fn indirect(
        &mut self,
        this: This,
        signature: This,
        max: u32,
        arguments: This,
        offset: u64,
        count: This,
        count_offset: u64,
    ) {
        self.hold(signature, false);
        self.hold(arguments, true);
        self.hold(count, true);
        self.mark(this, None);
        self.put(
            this,
            Call::Indirect {
                signature: signature as usize,
                max,
                arguments: arguments as usize,
                offset,
                count: count as usize,
                count_offset,
            },
        );
    }

    /// A draw (`compute` false), a dispatch (true) or either (`None`)
    /// follows: with `VICTORY_THIN_CHECK=1`, what its tables begin with now.
    pub(super) fn mark(&mut self, this: This, compute: Option<bool>) {
        if !crate::thin::check() {
            return;
        }
        let heaps = self.heaps;
        let handles: Vec<u64> = [false, true]
            .into_iter()
            .filter(|&kind| compute.is_none_or(|c| c == kind))
            .flat_map(|kind| self.tables[usize::from(kind)])
            .filter(|&handle| {
                handle != 0
                    && heaps.iter().any(|&h| h != 0 && crate::descriptor::heap(h as This).index(handle).is_some())
            })
            .collect();
        if handles.is_empty() {
            return;
        }
        let print = print(&handles);
        self.put_all(this, &handles, |at, count| Call::Mark { at, count, print });
    }
}

/// Makes the calls written down for the list `target`, in order: what the
/// list's methods did inside the title's call.
fn apply(target: usize, calls: &[Call], words: &[u64]) {
    let this = target as This;
    let Some(host) = super::host_list_now(this) else {
        return;
    };
    crate::lockmeter::set_class(crate::lockmeter::RECORDER);
    // SAFETY: one of our lists, alive while its stream holds calls (its
    // release drains first); nobody else is at its state now.
    let state = unsafe { &(*this.cast::<List>()).state };
    let mut state = crate::lockmeter::lock("li:state", state);
    let s = &mut *state;
    for call in calls {
        match *call {
            Call::Topology(topology) => s.set_topology(host, topology),
            Call::Viewports { at, count } => s.set_viewports(host, items::<D3D12_VIEWPORT>(words, at, count)),
            Call::Scissors { at, count } => s.set_scissors(host, items::<RECT>(words, at, count)),
            Call::Blend(factor) => s.set_blend_factor(host, factor.as_ref()),
            Call::StencilRef(reference) => s.set_stencil_ref(host, reference),
            Call::DepthBounds(min, max) => s.set_depth_bounds(host, min, max),
            Call::Heaps { at, count } => s.set_heaps(items::<This>(words, at, count)),
            Call::Table { compute, slot, handle } => s.set_table(compute, slot, handle),
            Call::Constant { compute, slot, value, offset } => s.set_constants(compute, slot, &[value], offset),
            Call::Constants { compute, slot, offset, at, count } => {
                s.set_constants(compute, slot, items::<u32>(words, at, count), offset)
            }
            Call::View { compute, slot, address } => s.set_view(compute, slot, address),
            Call::Index(view) => s.set_index_buffer(view.as_ref()),
            Call::Vertex { start, count, at, given } => {
                if given {
                    s.set_vertex_buffers(start, items::<VertexView>(words, at, count));
                } else {
                    s.clear_vertex_buffers(start, count);
                }
            }
            Call::Targets { records, count, present, depth } => {
                let place = |k: usize| (records + k * RECORD * 8) as u64;
                let mut handles = [0u64; 8];
                for (k, handle) in handles.iter_mut().enumerate().take(count as usize) {
                    if present >> k & 1 != 0 {
                        *handle = place(k);
                    }
                }
                s.set_targets(host, &handles[..(count as usize).min(8)], if depth { place(8) } else { 0 });
            }
            Call::Pipeline(pipeline) => s.set_pipeline(pipeline as This),
            Call::Draw { vertices, instances, first, first_instance } => {
                s.draw(host, vertices, instances, first, first_instance)
            }
            Call::DrawIndexed { indices, instances, first, base, first_instance } => {
                s.draw_indexed(host, indices, instances, first, base, first_instance)
            }
            Call::Dispatch { x, y, z } => s.dispatch(host, x, y, z),
            Call::DispatchX { x, y, z, flags } => s.dispatch_x(host, x, y, z, flags),
            Call::DispatchMesh { x } => s.dispatch_mesh(host, x),
            Call::Indirect { signature, max, arguments, offset, count, count_offset } => {
                super::execute_indirect_now(
                    this,
                    signature as This,
                    max,
                    arguments as This,
                    offset,
                    count as This,
                    count_offset,
                    Some((host, &mut *s)),
                );
            }
            Call::Mark { at, count, print: then } => {
                crate::thin::checked(print(items::<u64>(words, at, count)) != then);
            }
            Call::Barriers { at, count } => super::barrier_now(host, items::<super::Barrier>(words, at, count)),
            Call::BarrierGroups => super::barrier_groups_now(host),
            Call::Close => host.close(),
        }
    }
}
