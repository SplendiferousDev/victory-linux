//! The card's faults as the host's kernel reports them (Linux, under Wine).
//!
//! On Windows the display driver's errors reach the layer through the
//! system's event log (`crate::drvwatch`). Under Wine there is no such log:
//! the driver runs in the Linux kernel and reports there, NVIDIA as
//! `NVRM: Xid (PCI:…): 31, …` lines, amdgpu as `ring … timeout`, `GPU reset
//! begin!` or `page fault` lines. The layer cannot read the kernel's log
//! from inside Wine, so a small watcher on the host
//! (`scripts/linux/fault-watch.sh`, started by `scripts/linux/run.sh`)
//! appends each such line to a file, and the launcher names that file in
//! `VICTORY_HOST_FAULT_FILE` (a Windows path, e.g. on the `Z:` drive).
//!
//! Here a thread looks at the file's size every 250 ms; new lines are
//! parsed ([`parse`]), lines of one fault that arrive together are taken as
//! one event ([`Events`]), and each event is logged once and fed to
//! `crate::drvwatch` as if the system's event log had recorded it: so every
//! path that turns itself off at the driver's first error does so on Linux
//! too. What the file held when the layer started, and lines stamped before
//! that, are not events of this run.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The card's vendor, as the line names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Vendor {
    Nvidia,
    Amd,
}

/// One kernel line about a fault of the card.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Fault {
    pub vendor: Vendor,
    /// What happened, short: `Xid 31`, `ring timeout`, `GPU reset`,
    /// `page fault`.
    pub kind: String,
    /// The kernel's time stamp in seconds since 1970, if the line starts
    /// with one (`journalctl -o short-unix`).
    pub time: Option<f64>,
    /// The kernel's message, from the driver's tag on.
    pub text: String,
}

/// Reads one line of the watcher's file (or a raw kernel line): `None` for
/// anything that is no fault of the card (follow-up lines such as amdgpu's
/// `GPU reset(2) succeeded!` included).
pub(crate) fn parse(line: &str) -> Option<Fault> {
    let line = line.trim();
    // An optional leading time stamp ("1764100000.123456 ").
    let (time, rest) = match line.split_once(' ') {
        Some((first, rest))
            if first.contains('.') && first.bytes().all(|b| b.is_ascii_digit() || b == b'.') =>
        {
            (first.parse::<f64>().ok(), rest)
        }
        _ => (None, line),
    };
    if let Some(at) = rest.find("NVRM: Xid") {
        let text = &rest[at..];
        // "NVRM: Xid (PCI:0000:01:00): 31, pid=…" (older drivers: "(0000:01:00)").
        let after = &text[text.find("): ")? + 3..];
        let code: String = after.chars().take_while(char::is_ascii_digit).collect();
        if code.is_empty() {
            return None;
        }
        return Some(Fault {
            vendor: Vendor::Nvidia,
            kind: format!("Xid {code}"),
            time,
            text: text.to_owned(),
        });
    }
    // The driver failing inside itself, with no Xid: a mapping it could not
    // set up ("intermapRegisterDmaMapping: Failed to insert ...") and the
    // assertions that follow ("nvAssertFailedNoLog: ... gpu_vaspace.c").
    // These came before a hung kernel driver once, so they count as faults.
    for (needle, kind) in [
        ("NVRM: intermapRegisterDmaMapping: Failed", "mapping failed"),
        ("nvAssertFailed", "driver assertion"),
    ] {
        if let Some(at) = rest.find(needle) {
            let at = rest[..at].rfind("NVRM").unwrap_or(at);
            return Some(Fault {
                vendor: Vendor::Nvidia,
                kind: kind.to_owned(),
                time,
                text: rest[at..].to_owned(),
            });
        }
    }
    let at = rest.find("amdgpu")?;
    let text = &rest[at..];
    let kind = if text.contains("GPU reset begin") {
        "GPU reset"
    } else if text.contains(" timeout") && text.contains("ring ") {
        "ring timeout"
    } else if text.contains("page fault (") || text.ends_with("page fault") {
        "page fault"
    } else {
        return None;
    };
    Some(Fault {
        vendor: Vendor::Amd,
        kind: kind.to_owned(),
        time,
        text: text.to_owned(),
    })
}

/// Lines of one fault, taken as one event: a fault of the card writes
/// several lines (an NVIDIA Xid 13 one per warp and register, amdgpu a
/// page fault with its status registers), within milliseconds. A line of
/// the same vendor and kind within [`Events::SAME`] of the last one is part
/// of the same event.
#[derive(Default)]
pub(crate) struct Events {
    last: Vec<(Vendor, String, f64)>,
}

impl Events {
    /// Seconds between lines of one event.
    const SAME: f64 = 2.0;

    /// Whether `fault`, seen at `now` (seconds; the line's own stamp if it
    /// has one), starts a new event.
    pub(crate) fn is_new(&mut self, fault: &Fault, now: f64) -> bool {
        let at = fault.time.unwrap_or(now);
        match self
            .last
            .iter_mut()
            .find(|(v, k, _)| *v == fault.vendor && *k == fault.kind)
        {
            Some((_, _, last)) => {
                let new = (at - *last).abs() > Self::SAME;
                *last = at;
                new
            }
            None => {
                self.last.push((fault.vendor, fault.kind.clone(), at));
                true
            }
        }
    }
}

/// Follows a file that another process appends lines to.
pub(crate) struct Follow {
    offset: u64,
    partial: Vec<u8>,
}

impl Follow {
    /// From the file's present end on: what it already holds is not read.
    /// A file that does not exist yet is read from its start once it does.
    pub(crate) fn from_end(path: &Path) -> Self {
        Self {
            offset: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            partial: Vec::new(),
        }
    }

    /// The whole lines appended since the last call. Cheap when nothing was
    /// (one size query). A file that got shorter was made anew: it is read
    /// from its start.
    pub(crate) fn poll(&mut self, path: &Path) -> Vec<String> {
        let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else {
            return Vec::new();
        };
        if len < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset {
            return Vec::new();
        }
        let mut got = Vec::new();
        let read = std::fs::File::open(path).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.offset))?;
            f.take(len - self.offset).read_to_end(&mut got)
        });
        let Ok(n) = read else {
            return Vec::new();
        };
        self.offset += n as u64;
        self.partial.extend_from_slice(&got);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return Vec::new();
        };
        let whole: Vec<u8> = self.partial.drain(..=end).collect();
        String::from_utf8_lossy(&whole)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Starts following `VICTORY_HOST_FAULT_FILE`, once, if it is set.
pub(crate) fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(path) = std::env::var_os("VICTORY_HOST_FAULT_FILE").filter(|v| !v.is_empty()) else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let started = unix_now();
        let mut follow = Follow::from_end(&path);
        let shown = path.display().to_string();
        let made = std::thread::Builder::new().name("victory-hostfault".into()).spawn(move || {
            let mut events = Events::default();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                for line in follow.poll(&path) {
                    let Some(fault) = parse(&line) else { continue };
                    // A stale file: lines stamped before the layer started
                    // (a second of slack for the two clocks).
                    if fault.time.is_some_and(|t| t < started - 1.0) {
                        continue;
                    }
                    if !events.is_new(&fault, unix_now()) {
                        continue;
                    }
                    let first = crate::drvwatch::report();
                    tracing::warn!(
                        vendor = ?fault.vendor,
                        "the host's kernel reported a fault of the card ({}): {}{}",
                        fault.kind,
                        fault.text.chars().take(200).collect::<String>(),
                        if first { "; from now on the paths under suspicion are off for this run" } else { "" }
                    );
                }
            }
        });
        match made {
            Ok(_) => tracing::info!(
                file = %shown,
                "the card's faults reported by the host's kernel are watched (VICTORY_HOST_FAULT_FILE): each is logged here, and \
                 the first turns the paths under suspicion off"
            ),
            Err(e) => tracing::warn!(error = %e, "the host's fault file is not watched: no thread"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn nvidia_xid_lines_are_read() {
        let f = parse(
            "1764100000.250000 NVRM: Xid (PCI:0000:01:00): 31, pid=4242, name=victory.exe, Ch 00000010, intr 00000000. MMU Fault: ENGINE GRAPHICS",
        )
        .unwrap();
        assert_eq!(f.vendor, Vendor::Nvidia);
        assert_eq!(f.kind, "Xid 31");
        let m = parse("1764100000.5 NVRM: intermapRegisterDmaMapping: Failed to insert new mapping node for range 0x10000-0x1FFFF!").unwrap();
        assert_eq!(m.kind, "mapping failed");
        let a = parse("box kernel: NVRM: GPU0 nvAssertFailedNoLog: Assertion failed: NV_OK == status @ gpu_vaspace.c:4155").unwrap();
        assert_eq!(a.kind, "driver assertion");
        assert!(a.text.starts_with("NVRM: GPU0"));
        assert_eq!(f.time, Some(1764100000.25));
        assert!(f.text.starts_with("NVRM: Xid"));
        // Raw journal line with a host name, no stamp of ours.
        let f = parse("box kernel: NVRM: Xid (PCI:0000:01:00): 13, pid=1, name=a, Graphics Exception: ESR 0x0=0x0").unwrap();
        assert_eq!((f.kind.as_str(), f.time), ("Xid 13", None));
        // Older drivers' address form.
        assert_eq!(
            parse("NVRM: Xid (0000:01:00): 79, GPU has fallen off the bus.")
                .unwrap()
                .kind,
            "Xid 79"
        );
        assert_eq!(
            parse("NVRM: Xid (PCI:0000:01:00): 109, pid=1, name=a, Ch 00000001")
                .unwrap()
                .kind,
            "Xid 109"
        );
        // Other NVRM lines are no faults.
        assert_eq!(
            parse("1764100000.1 NVRM: GPU at PCI:0000:01:00: GPU-0000"),
            None
        );
        assert_eq!(parse("NVRM: Xid (PCI:0000:01:00): , garbled"), None);
    }

    #[test]
    fn amdgpu_lines_are_read() {
        let kind = |l: &str| parse(l).map(|f| (f.vendor, f.kind));
        let amd = |k: &str| Some((Vendor::Amd, k.to_owned()));
        assert_eq!(
            kind(
                "1764100000.5 [drm:amdgpu_job_timedout [amdgpu]] *ERROR* ring gfx_0.0.0 timeout, signaled seq=10, emitted seq=12"
            ),
            amd("ring timeout")
        );
        assert_eq!(
            kind("amdgpu 0000:03:00.0: amdgpu: ring comp_1.0.0 timeout, but soft recovered"),
            amd("ring timeout")
        );
        assert_eq!(
            kind("1764100000.6 amdgpu 0000:03:00.0: amdgpu: GPU reset begin!"),
            amd("GPU reset")
        );
        assert_eq!(
            kind(
                "amdgpu 0000:03:00.0: amdgpu: [gfxhub] page fault (src_id:0 ring:24 vmid:3 pasid:32769)"
            ),
            amd("page fault")
        );
        // Follow-ups and harmless lines.
        assert_eq!(
            kind("amdgpu 0000:03:00.0: amdgpu: GPU reset(2) succeeded!"),
            None
        );
        assert_eq!(
            kind(
                "amdgpu 0000:03:00.0: amdgpu:   in page starting at address 0x0000800100200000 from client 10"
            ),
            None
        );
        assert_eq!(
            kind("amdgpu 0000:03:00.0: amdgpu: SMU is initialized successfully!"),
            None
        );
        assert_eq!(kind("usb 1-2: new high-speed USB device"), None);
        assert_eq!(kind(""), None);
    }

    #[test]
    fn lines_of_one_fault_are_one_event() {
        let mut events = Events::default();
        let at = |s: &str, t: f64| Fault {
            time: Some(t),
            ..parse(s).unwrap()
        };
        let x13 = "NVRM: Xid (PCI:0000:01:00): 13, pid=1, name=a, Graphics Exception";
        let x31 = "NVRM: Xid (PCI:0000:01:00): 31, pid=1, name=a, MMU Fault";
        assert!(events.is_new(&at(x13, 100.0), 0.0));
        assert!(!events.is_new(&at(x13, 100.01), 0.0));
        assert!(!events.is_new(&at(x13, 101.5), 0.0)); // within 2 s of the last line
        assert!(events.is_new(&at(x31, 101.6), 0.0)); // another kind
        assert!(events.is_new(&at(x13, 110.0), 0.0)); // a later fault
        // Without stamps, the time it was seen.
        let plain = parse("amdgpu 0000:03:00.0: amdgpu: GPU reset begin!").unwrap();
        assert!(events.is_new(&plain, 50.0));
        assert!(!events.is_new(&plain, 50.5));
        assert!(events.is_new(&plain, 60.0));
    }

    #[test]
    fn the_file_is_followed_from_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("faults.txt");
        std::fs::write(
            &path,
            "1.0 NVRM: Xid (PCI:0000:01:00): 31, before the start\n",
        )
        .unwrap();
        let mut follow = Follow::from_end(&path);
        assert!(
            follow.poll(&path).is_empty(),
            "what the file held at the start is not read"
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"2.0 first\n2.1 sec").unwrap();
        f.flush().unwrap();
        assert_eq!(
            follow.poll(&path),
            ["2.0 first"],
            "a line not yet ended waits"
        );
        f.write_all(b"ond\n\n3.0 third\n").unwrap();
        f.flush().unwrap();
        assert_eq!(follow.poll(&path), ["2.1 second", "3.0 third"]);
        assert!(follow.poll(&path).is_empty());
        drop(f);
        // Made anew (shorter): read from its start.
        std::fs::write(&path, "4.0 new\n").unwrap();
        assert_eq!(follow.poll(&path), ["4.0 new"]);
    }

    #[test]
    fn a_file_made_later_is_read_from_its_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("later.txt");
        let mut follow = Follow::from_end(&path);
        assert!(follow.poll(&path).is_empty());
        std::fs::write(&path, "5.0 one\n").unwrap();
        assert_eq!(follow.poll(&path), ["5.0 one"]);
    }
}
