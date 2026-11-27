//! The frame line's report of the line of flights (`late_line`), split out
//! of `host` to keep that file under the size limit.

use super::*;

/// For the frame-rate line, with [`late`]: what the line of flights did
/// per frame since the last one.
pub(super) fn late_line(frames: f64) {
    if !late() {
        return;
    }
    #[cfg(feature = "diagnostics")]
    fill_check::line();
    fill_ahead::line(frames);
    let s: Vec<f64> = LATE_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
         settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
         title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
         a texture {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
         bottom-of-pipe writes (VICTORY_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}; \
         {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
        s[0],
        s[1],
        if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
        s[12] / 1000.0,
        s[WHY_SYNC],
        s[WHY_CARD_DONE],
        s[WHY_VALUE],
        s[WHY_STALE],
        s[WHY_READ],
        s[15],
        s[16],
        s[17],
        s[WHY_RUN],
        late_most(),
        s[WHY_MOST],
        s[WHY_WRITES],
        s[WHY_FENCE],
        s[13],
        s[14],
    );
    {
        let w: Vec<f64> = LATE_WAITED.iter().map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames).collect();
        tracing::info!(
            "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
             the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
             title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}",
            w[WHY_SYNC],
            w[WHY_CARD_DONE],
            w[WHY_VALUE],
            w[WHY_STALE],
            w[WHY_READ],
            w[WHY_RUN],
            w[WHY_MOST],
            w[WHY_WRITES],
            w[WHY_FENCE],
        );
    }
    if relay() {
        let r: Vec<f64> = RELAY_STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        let [again, noted] = [0, 1].map(|i| BESIDE_STATS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        tracing::info!(
            "the relay per frame (VICTORY_RELAY={}): {:.1} flights landed in {:.1} landings by the thread that copies back, \
             beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
             fetches of card copies' pieces, and was {:.1} ms landing in all (the frame line's \"copying back\" is its time; \
             \"settles\" above are waits of other threads for it); pieces fetched on the copy queue beside later \
             submissions: {noted:.1}, pieces those fetched again at their own landing whatever the compare said: {again:.1} \
             (VICTORY_QUEUE2_REFETCH)",
            if relay_exact() { 2 } else { 1 },
            r[1],
            r[0],
            r[2] / 1000.0,
            r[3] / 1000.0,
            r[4] / 1000.0,
        );
        if inflight_fetch() {
            let i: Vec<f64> = INFLIGHT_STATS.iter().map(|i| i.swap(0, Ordering::Relaxed) as f64 / frames).collect();
            tracing::info!(
                "card copies' pieces fetched inside their flights per frame (VICTORY_CARD_FETCH_INFLIGHT=1): {:.1} pieces \
                 ({:.2} MB) merged with no fetch of their own; {:.1} landings fetched the old way (the wait above), {:.1} of \
                 them because a flight's pieces overflowed the slots",
                i[0],
                i[1] / (1 << 20) as f64,
                i[3],
                i[2],
            );
        }
    }
    if late_exact() {
        let [pages, runs, bytes] = crate::shadow::merged_stats().map(|n| n as f64 / frames);
        let gave_up = crate::shadow::merge_gave_up();
        tracing::info!(
            "title bytes put in under flights' shader writes per frame (VICTORY_LATE_WAIT=2): {pages:.0} pages looked at, \
             {runs:.0} changed runs, {:.2} MB; in these frames the merge gave up (the flights are then settled whole): {} times \
             because the copy's memory is not watched, {} because the write tracking had no answer, {} for more changed runs than \
             VICTORY_MERGE_RUNS leaves, {} for a copy in PC memory without its snapshot",
            bytes / (1 << 20) as f64,
            gave_up[0],
            gave_up[1],
            gave_up[2],
            gave_up[3]
        );
        if crate::shadow::masked_patch_mode() != 0 {
            let p = [0, 1, 2, 3].map(|i| crate::shadow::PATCHED[i].swap(0, Ordering::Relaxed));
            tracing::info!(
                "masked patches per frame (VICTORY_MASKED_PATCH): {:.1} pages past the merge's cap patched instead of a settle, \
                 {:.0} dwords, {:.1} dispatches in {:.1} submissions",
                p[0] as f64 / frames,
                p[1] as f64 / frames,
                p[2] as f64 / frames,
                p[3] as f64 / frames
            );
            if crate::shadow::patch_lean_mode() != 0 {
                let lean = [0, 1].map(|i| crate::shadow::PATCH_LEAN[i].swap(0, Ordering::Relaxed) as f64 / frames);
                let made = [0, 1].map(|i| crate::shadow::PATCH_LEAN_BUFFERS[i].swap(0, Ordering::Relaxed) as f64 / frames);
                tracing::info!(
                    "the masked patch lean (VICTORY_PATCH_LEAN), per frame: {:.1} pages under the cap patched for their many runs                      ({:.0} runs that would have gone up one by one); buffers of words {:.1} made, {:.1} kept ones used again",
                    lean[0],
                    lean[1],
                    made[0],
                    made[1]
                );
            }
            if crate::patchspan::set() {
                let m = [0, 1, 2, 3, 4].map(|i| crate::patchspan::METER[i].swap(0, Ordering::Relaxed) as f64 / frames);
                tracing::info!(
                    "the masked patch by spans (VICTORY_PATCH_SPANS), per frame: {:.1} pages patched in one pass, {:.1} spans noted as sent, \
                     {:.0} words, {:.2} ms making them and noting the pages, {:.0} exact runs read off spans by landings",
                    m[0],
                    m[1],
                    m[2],
                    m[3] / 1e6,
                    m[4]
                );
            }
        }
    }
    {
        let found = [0, 1, 2, 3].map(|i| crate::shadow::LOOKUPS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        tracing::info!(
            "asks for a buffer's host copy per frame: {:.0} answered from the asking thread's memory of them, {:.0} from the tables (VICTORY_LOOKUP_CACHE); \
             {:.1} changes of the tables, each emptying every thread's memory of its asks, found {:.1} times",
            found[0],
            found[1],
            found[2],
            found[3]
        );
    }
    {
        let parts = [0, 1, 2, 3, 4, 5].map(|i| PREP_PARTS[i].swap(0, Ordering::Relaxed) as f64 / 1e3 / frames);
        tracing::info!(
            "a submission's preparing by part, ms per frame: the write-watch asking and the look for title bytes under flights \
             (settles for them included) {:.1}; the lists' steps (title copies, fills, preludes; refreshes are only noted) {:.1}; \
             the refreshes (the flush) {:.1}; snapshots and the wait for card results {:.1}; the card copies' send and hash lists \
             (card_before) {:.1}; handing the lists to the card and signalling {:.1}",
            parts[0],
            parts[1],
            parts[2],
            parts[3],
            parts[4],
            parts[5]
        );
        if crate::shadow::merge_on_card_wanted() {
            let [submissions, dispatches, mask, copies, plain, past] =
                [0, 1, 2, 3, 4, 5].map(|i| CARD_MERGES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            let torn = crate::mergecap::taken(frames);
            let qsync = crate::mergecap::qsync_taken(frames);
            let conflicts = crate::threeway::taken(frames);
            tracing::info!(
                "of them through the merge on the card per frame (VICTORY_MERGE_ON_CARD=1 turned it on): {submissions:.1} submissions with \
                 {dispatches:.1} dispatches and {:.0} KiB of masks; {copies:.1} submissions sent theirs up as copies; plain CPU sides \
                 (VICTORY_CARD_TWIN_PLAIN): {:.0} KiB of title bytes beside the masks, {past:.1} copies sent as copies (past the most a submission); \
                 MERGE TORN WORDS (a title byte merged beside a shader's byte) in {:.1} landed pieces: {:.1} of 4 bytes, {:.1} of 8, {:.1} of 16; \
                 cap of changed runs (VICTORY_MERGE_CAP) {}; VICTORY_MERGE_QSYNC: {:.1} merges waited for copy-queue fetches, {:.1} fetches behind a merge; \
                 MERGE CONFLICT BYTES (a shader wrote a byte the title changed; VICTORY_MERGE_THREEWAY {}): {:.1} seen at landings, {:.1} kept for the shader, \
                 {:.0} KiB of slot tables and old bytes sent up",
                mask / 1024.0,
                plain / 1024.0,
                torn[0],
                torn[1],
                torn[2],
                torn[3],
                crate::mergecap::cap_said(),
                qsync[0],
                qsync[1],
                if crate::threeway::on() { "on: the shader's value kept" } else { "off: the title's value won" },
                conflicts[0],
                conflicts[1],
                conflicts[2] / 1024.0
            );
        }
        let card = [0, 1, 2, 3, 4, 5, 6].map(|i| CARD_BEFORE_PARTS[i].swap(0, Ordering::Relaxed) as f64 / 1e3 / frames);
        tracing::info!(
            "card_before by part, ms per frame: the ranges given to shaders gathered {:.1}; the changed ranges and masked patches \
             taken (the queue-order lock's wait included) {:.1}; the patch's buffer made or reused and filled {:.1}; the send list's \
             copies recorded {:.1}; its hash passes recorded {:.1}; the compare list {:.1}; the send list closed and handed over {:.1}",
            card[0],
            card[1],
            card[2],
            card[3],
            card[4],
            card[5],
            card[6]
        );
        #[cfg(feature = "diagnostics")]
        {
            let k = [0, 1, 2, 3, 4, 5, 6].map(|i| {
                (
                    STEP_KINDS[i][0].swap(0, Ordering::Relaxed) as f64 / frames,
                    STEP_KINDS[i][1].swap(0, Ordering::Relaxed) as f64 / 1e3 / frames,
                )
            });
            let f = [0, 1, 2, 3, 4].map(|i| STEP_FLUSHES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            tracing::info!(
                "the lists' steps by kind, per frame (count, ms): title copies {:.0} {:.1}; fills {:.0} {:.1}; refreshes noted {:.0} (counted, not timed); \
                 memory writes of ours (with the flush they set off) {:.0} {:.1}; host steps {:.0} {:.1}; ranges given to shaders (with \
                 their copy's refresh) {:.0} {:.1}; textures noted {:.0} {:.1}; flushes of refreshes, the submission's last ones \
                 included: {:.0} in {:.1} ms bringing {:.0} copies up to date; texture checks {:.0} in {:.1} ms",
                k[0].0,
                k[0].1,
                k[1].0,
                k[1].1,
                k[2].0,
                k[3].0,
                k[3].1,
                k[4].0,
                k[4].1,
                k[5].0,
                k[5].1,
                k[6].0,
                k[6].1,
                f[0],
                f[1] / 1e3,
                f[2],
                f[3],
                f[4] / 1e3
            );
        }
    }
}
