//! The title's thread pinnings: see `victory_gpu::placement`.

use victory_gpu::placement::{GetThreadGroupAffinity, SetThreadAffinityMask, SetThreadGroupAffinity};

use crate::{HleModule, hle_export};

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "placement",
        dlls: &["kernel32.dll", "kernelbase.dll", "api-ms-win-core-processtopology-*"],
        exports: vec![
            hle_export!(SetThreadGroupAffinity),
            hle_export!(SetThreadAffinityMask),
            hle_export!(GetThreadGroupAffinity),
        ],
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_title_s_pinnings_come_here() {
        for (dll, name) in [
            ("api-ms-win-core-processtopology-l1-1-0.dll", "SetThreadGroupAffinity"),
            ("api-ms-win-core-processtopology-l1-1-0.dll", "GetThreadGroupAffinity"),
            ("api-ms-win-core-processtopology-obsolete-l1-1-0.dll", "SetThreadAffinityMask"),
            ("kernel32.dll", "SetThreadAffinityMask"),
        ] {
            assert!(crate::registry::lookup(dll, name).is_some(), "{dll} {name}");
        }
    }
}
