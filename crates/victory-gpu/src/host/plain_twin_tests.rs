use super::*;

/// `memory[to] = memory[from] + add`.
const ADD: &str = r#"
#define RS "RootConstants(num32BitConstants=3, b0), UAV(u0)"
cbuffer What : register(b0) { uint to; uint from; uint add; };
RWByteAddressBuffer memory : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    memory.Store(to, memory.Load(from) + add);
}
"#;

#[test]
fn plain_cpu_sides_go_up_through_the_ring_and_shader_writes_come_back() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
    const MEM_WRITE_WATCH: u32 = 0x0020_0000;
    let Some(host) = host() else {
        return;
    };
    if !crate::shadow::cards() {
        eprintln!("plain_twin_tests: skipped, copies in the card's memory are off (VICTORY_CARD_BUFFERS=0)");
        return;
    }
    const PIECE: usize = 64 << 10;
    // 12 MiB through a ring of one 4 MiB chunk: the first send comes
    // round twice.
    const LEN: usize = 12 << 20;
    const ROUNDS: usize = 12;
    // SAFETY: a plain allocation of our own.
    let title = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) } as *mut u8;
    assert!(!title.is_null());
    let dword = |at: usize| unsafe { title.add(at).cast::<u32>() };
    let pattern = |i: usize| 0x3C00_0000 ^ (i as u32).wrapping_mul(0x9E37_79B9);
    for i in 0..LEN / 4 {
        // SAFETY: inside the allocation.
        unsafe { dword(i * 4).write_volatile(pattern(i)) };
    }
    crate::shadow::PLAIN_FORCED.store(2, Ordering::Relaxed);
    crate::shadow::TWIN_RING_FORCED.store(SCRATCH_CHUNK, Ordering::Relaxed);
    crate::shadow::register(0x71_7171, title as u64, LEN as u64);
    let shadow = crate::shadow::find(title as u64, LEN as u64).expect("a host copy");
    crate::shadow::PLAIN_FORCED.store(0, Ordering::Relaxed);
    if !shadow.carded() {
        crate::shadow::TWIN_RING_FORCED.store(0, Ordering::Relaxed);
        eprintln!("plain_twin_tests: skipped, the copy is not in the card's memory (no room, or no compare shader)");
        return;
    }
    assert!(
        shadow.card_sides().is_some_and(|(cpu, _, _)| cpu.is_none()),
        "the copy's CPU side is a buffer, not plain memory"
    );
    let (plain, _) = crate::shadow::plain_held();
    assert!(plain[0] >= 1 && plain[1] >= LEN as u64, "plain memory not counted: {plain:?}");
    let code = compile(ADD, windows::core::s!("main"), windows::core::s!("cs_5_1")).expect("the test's shader");
    // SAFETY: the shader's own root signature; plain creation.
    let (root, pso): (ID3D12RootSignature, ID3D12PipelineState) = unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).expect("a root signature");
        let pso = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
            .expect("a pipeline");
        (root, pso)
    };
    let waits_before = TWIN_RING[2].load(Ordering::Relaxed);
    // Round k: the title writes a dword spread over the whole copy (in
    // each 4 MiB of it); the shader adds k and stores the sum in a
    // declared piece near the end.
    let from = |k: usize| (1 + k * 15) * PIECE + 4 * k + 64;
    let to = |k: usize| (170 + k) * PIECE + 8 * k + 128;
    let titles = |k: usize| 0x0200_0000 + 0x1000 * k as u32 + 9;
    let mut lists = Vec::new();
    for k in 0..ROUNDS {
        // SAFETY: inside the allocation.
        unsafe { dword(from(k)).write_volatile(titles(k)) };
        let list = host.new_list().expect("a host list");
        shadow.refresh(&list, title as u64, LEN as u64);
        shadow.gpu_writes(&list, (title as usize + (170 + k) * PIECE) as u64, PIECE as u64);
        // SAFETY: an open list; the copy's buffer, with unordered
        // access; the shader's own root signature.
        unsafe {
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pso);
            let constants = [to(k) as u32, from(k) as u32, k as u32];
            list.list.SetComputeRoot32BitConstants(0, 3, constants.as_ptr().cast(), 0);
            list.list.SetComputeRootUnorderedAccessView(1, shadow.buffer.GetGPUVirtualAddress());
            list.list.Dispatch(1, 1, 1);
        }
        list.close();
        if late() {
            host.execute_own(&list);
        } else {
            host.execute(&[&list]);
        }
        lists.push(list);
    }
    settle_now(WHY_SYNC);
    crate::shadow::TWIN_RING_FORCED.store(0, Ordering::Relaxed);
    let waits = TWIN_RING[2].load(Ordering::Relaxed) - waits_before;
    eprintln!("plain_twin_tests: {waits} waits for a full ring (late {}, relay {})", late(), relay());
    assert!(waits >= 2, "the first send of 12 MiB through a 4 MiB ring did not come round: {waits} waits");
    for k in 0..ROUNDS {
        // SAFETY: inside the allocation.
        let (sum, kept) = unsafe { (dword(to(k)).read_volatile(), dword(from(k)).read_volatile()) };
        assert_eq!(kept, titles(k), "round {k}: the title's own dword changed");
        assert_eq!(
            sum,
            titles(k) + k as u32,
            "round {k}: the shader read {:#x} where the title had written {:#x}, or its write did not come back",
            sum.wrapping_sub(k as u32),
            titles(k)
        );
    }
    let touched: std::collections::HashSet<usize> = (0..ROUNDS).flat_map(|k| [from(k), to(k)]).collect();
    for i in (0..LEN / 4).filter(|i| !touched.contains(&(i * 4))) {
        // SAFETY: inside the allocation.
        let got = unsafe { dword(i * 4).read_volatile() };
        assert_eq!(got, pattern(i), "dword {i} of title memory changed");
    }
    // (The title's buffer stays registered, as the other tests leave
    // theirs, and its memory with it.)
    drop(lists);
}
