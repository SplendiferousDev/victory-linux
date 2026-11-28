//! Slot marks (`slot_mark`) in translations of synthetic programs: stored
//! only with the switch on and only where the host cannot know the slot;
//! nothing of a translation changes while it is off. No device is used.

use super::container::{self, SigElement};
use super::slot_mark::{self, DISCARDS, EARLY, EARLY_OK, MARKS, Pixel, Settings, slot_marks_of};
use super::{HostShader, emit, lower};
use crate::dxbc::{Container, FourCC};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
/// The marks' buffer in these tests.
const SLOT: u32 = 77;

fn param(root: RootType, user_data: u8) -> RootParam {
    RootParam { root, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data, slot: user_data }
}

/// A table's range of 2D textures at its start.
fn textures() -> UsedResource {
    UsedResource {
        kind: ResourceType::Srv,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 8,
        user_data: 0,
        slot: 0,
    }
}

/// Runs `f` with the switches as `settings` on this thread.
fn with<R>(settings: Option<Settings>, f: impl FnOnce() -> R) -> R {
    slot_mark::TEST.with(|t| t.set(settings.map(|s| (s, SLOT))));
    let out = f();
    slot_mark::TEST.with(|t| t.set(None));
    out
}

fn on(pixel: Pixel, tables: bool) -> Option<Settings> {
    Some(Settings { on: true, pixel, tables })
}

/// A compute program that samples the texture whose descriptor it loads
/// from the table at s0: at dword 0 of the table, or with `picked` at the
/// entry the root constant s1 numbers.
fn compute(picked: bool) -> Vec<u8> {
    let mut words = vec![
        0xBE84_0300u32, // s_mov_b32 s4, s0 (the table)
        0xBE85_0380,    // s_mov_b32 s5, 0
        0x8F14_8501,    // s_lshl_b32 s20, s1, 5 (a root constant, as an offset)
        0xF40C_0302,
    ];
    // s_load_dwordx8 s[12:19], s[4:5], s20 or 0
    words.push(if picked { 0x2800_0000 } else { 0xFA00_0000 });
    words.extend([
        0xBE88_0380, // s_mov_b32 s8, 0 (the sampler's words)
        0xBE89_0380,
        0xBE8A_0380,
        0xBE8B_0380,
        0xF09C_0F08,
        0x0043_0C02, // image_sample_lz v[12:15], v[2:3], s[12:19], s[8:11]
        0xD834_0000,
        0x0000_0C00, // ds_write_b32 v0, v12
        S_ENDPGM,
    ]);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 256,
        layout: vec![param(RootType::Table, 0), param(RootType::Constants, 1)],
        used: vec![textures()],
        io: StageIo::Cs(ComputeIo { rsrc2: 1 << 15 | 2 << 1, threads: [1, 1, 1] }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, false, 256).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
    })
    .unwrap()
    .dxil
}

/// A pixel program that samples the texture at the start of the table at
/// s0, or with `picked` the one at the start of the view heap, and writes
/// it to its target.
fn pixel(picked: bool) -> HostShader {
    let words = [
        0xBE81_0380u32, // s_mov_b32 s1, 0
        0xF40C_0000,
        0xFA00_0000, // s_load_dwordx8 s[0:7], s[0:1], null
        0xBE88_0380, // s_mov_b32 s8, 0 (the sampler's words)
        0xBE89_0380,
        0xBE8A_0380,
        0xBE8B_0380,
        0x7E02_02F0, // v_mov_b32 v1, 0.5
        0x7E04_02F0, // v_mov_b32 v2, 0.5
        0xF080_0108,
        0x0040_0301, // image_sample v3, v[1:2], s[0:7], s[8:11]
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xF800_180F,
        0x0303_0303, // exp mrt0, v3, v3, v3, v3 done vm
        S_ENDPGM,
    ];
    let (layout, used) = if picked {
        (vec![param(RootType::ResourceHeap, 0)], vec![])
    } else {
        (vec![param(RootType::Table, 0)], vec![textures()])
    };
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 4,
        layout,
        used,
        io: StageIo::Ps { inputs: vec![], input_addr: 0, db_shader_control: 0, patches: vec![] },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let target = SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    };
    let osg1 = container::signature(&[target], false);
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    emit(&c, &native, &p, &st, &io, &res).unwrap()
}

/// The feature bits of a translation's `SFI0` part.
fn features(dxil: &[u8]) -> u64 {
    let c = Container::parse(dxil).unwrap();
    u64::from_le_bytes(c.part(FourCC::SFI0).unwrap().data.try_into().unwrap())
}

/// `dxil.dll`, where there is one, accepts the translation. With
/// `SLOT_MARK_TEST_DUMP=<folder>` it is also written there as `name`, for a
/// converter outside the tests.
fn validate(name: &str, dxil: &[u8]) {
    #[cfg(windows)]
    if let Ok(mut v) = super::validator::Validator::load(super::validator::Validator::default_path().as_deref()) {
        v.validate(&mut dxil.to_vec()).expect("DXIL validation failed");
    }
    if let Some(dir) = std::env::var_os("SLOT_MARK_TEST_DUMP") {
        let _ = std::fs::write(std::path::PathBuf::from(dir).join(format!("{name}.dxil")), dxil);
    }
}

#[test]
fn a_fixed_table_place_is_never_marked_and_the_switch_off_changes_nothing() {
    let off = with(None, || compute(false));
    assert_eq!(slot_marks_of(&off), (0, 0));
    // The host reads the table for the draw: no mark, and the very same
    // translation as without the switch.
    let fixed = with(on(Pixel::All, false), || compute(false));
    assert_eq!(fixed, off);
    // Switched off by its settings, a picked slot is not marked either.
    let picked_off = with(None, || compute(true));
    assert_eq!(slot_marks_of(&picked_off), (0, 0));
    let unset = with(Some(Settings { on: false, pixel: Pixel::All, tables: true }), || compute(true));
    assert_eq!(unset, picked_off);
}

#[test]
fn a_slot_picked_at_run_time_is_marked_once_per_place() {
    let off = with(None, || compute(true));
    let marked = with(on(Pixel::All, false), || compute(true));
    assert_ne!(marked, off, "the store is in the translation");
    assert_eq!(slot_marks_of(&marked), (MARKS, 1));
    // A compute program declares nothing new for it.
    assert_eq!(features(&marked), features(&off));
    validate("compute-marked", &marked);
    validate("compute-plain", &off);
    // The pixel programs' setting is not a compute program's.
    assert_eq!(with(on(Pixel::None, false), || compute(true)), marked);
}

#[test]
fn fixed_table_places_are_marked_too_when_asked() {
    let off = with(None, || compute(false));
    let marked = with(on(Pixel::All, true), || compute(false));
    assert_ne!(marked, off);
    assert_eq!(slot_marks_of(&marked), (MARKS, 1));
}

#[test]
fn a_pixel_program_marks_and_tests_early_only_when_asked() {
    let off = with(None, || pixel(true)).dxil;
    assert_eq!(slot_marks_of(&off), (0, 0));
    let fixed = with(on(Pixel::All, false), || pixel(false)).dxil;
    assert_eq!(fixed, with(None, || pixel(false)).dxil, "a fixed table place: as without the switch");
    let marked = with(on(Pixel::All, false), || pixel(true)).dxil;
    // It neither discards nor writes depth: the early test would be exact.
    assert_eq!(slot_marks_of(&marked), (MARKS | EARLY_OK, 1));
    assert_eq!(features(&marked) & 0x2, 0, "not declared unless asked for");
    validate("pixel-marked", &marked);
    validate("pixel-plain", &off);
    let early = with(on(Pixel::All, false), || slot_mark::with_early_depth(true, || pixel(true))).dxil;
    assert_eq!(slot_marks_of(&early), (MARKS | EARLY_OK | EARLY, 1));
    assert_ne!(features(&early) & 0x2, 0, "the early depth and stencil test is declared");
    assert_eq!(slot_marks_of(&early).0 & DISCARDS, 0);
    validate("pixel-marked-early", &early);
    // Asked for without the switch, or for a program without marks: as ever.
    assert_eq!(with(None, || slot_mark::with_early_depth(true, || pixel(true))).dxil, off);
    assert_eq!(with(on(Pixel::All, false), || slot_mark::with_early_depth(true, || pixel(false))).dxil, fixed);
    // No pixel program marks: as without the switch. Only the free ones:
    // this one is.
    assert_eq!(with(on(Pixel::None, false), || pixel(true)).dxil, off);
    assert_eq!(with(on(Pixel::Free, false), || pixel(true)).dxil, marked);
}
