//! Rust target triples and the target predicates each one establishes.

/// One Rust target triple and the values of its target predicates.
///
/// A target decides `unix`, `windows`, `target_family`, `target_os`, `target_arch`,
/// `target_pointer_width`, `target_endian`, `target_env` and `target_vendor`. Every other
/// predicate stays unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustTarget {
    triple: &'static str,
    arch: &'static str,
    vendor: &'static str,
    os: &'static str,
    env: &'static str,
    family: &'static str,
    pointer_width: &'static str,
}

/// Supported targets as `rustc --print cfg --target <triple>` reports them. Every target
/// is little-endian.
const TARGETS: [RustTarget; 16] = [
    linux("x86_64-unknown-linux-gnu", "x86_64", "gnu", "64"),
    linux("aarch64-unknown-linux-gnu", "aarch64", "gnu", "64"),
    linux("x86_64-unknown-linux-musl", "x86_64", "musl", "64"),
    linux("aarch64-unknown-linux-musl", "aarch64", "musl", "64"),
    linux("i686-unknown-linux-gnu", "x86", "gnu", "32"),
    linux("armv7-unknown-linux-gnueabihf", "arm", "gnu", "32"),
    linux("riscv64gc-unknown-linux-gnu", "riscv64", "gnu", "64"),
    macos("x86_64-apple-darwin", "x86_64"),
    macos("aarch64-apple-darwin", "aarch64"),
    windows("x86_64-pc-windows-msvc", "x86_64", "msvc", "64"),
    windows("aarch64-pc-windows-msvc", "aarch64", "msvc", "64"),
    windows("i686-pc-windows-msvc", "x86", "msvc", "32"),
    windows("x86_64-pc-windows-gnu", "x86_64", "gnu", "64"),
    RustTarget {
        triple: "x86_64-unknown-freebsd",
        arch: "x86_64",
        vendor: "unknown",
        os: "freebsd",
        env: "",
        family: "unix",
        pointer_width: "64",
    },
    wasm("wasm32-unknown-unknown", "unknown", ""),
    wasm("wasm32-wasip1", "wasi", "p1"),
];

const fn linux(
    triple: &'static str,
    arch: &'static str,
    env: &'static str,
    pointer_width: &'static str,
) -> RustTarget {
    RustTarget {
        triple,
        arch,
        vendor: "unknown",
        os: "linux",
        env,
        family: "unix",
        pointer_width,
    }
}

const fn macos(triple: &'static str, arch: &'static str) -> RustTarget {
    RustTarget {
        triple,
        arch,
        vendor: "apple",
        os: "macos",
        env: "",
        family: "unix",
        pointer_width: "64",
    }
}

const fn windows(
    triple: &'static str,
    arch: &'static str,
    env: &'static str,
    pointer_width: &'static str,
) -> RustTarget {
    RustTarget {
        triple,
        arch,
        vendor: "pc",
        os: "windows",
        env,
        family: "windows",
        pointer_width,
    }
}

const fn wasm(triple: &'static str, os: &'static str, env: &'static str) -> RustTarget {
    RustTarget {
        triple,
        arch: "wasm32",
        vendor: "unknown",
        os,
        env,
        family: "wasm",
        pointer_width: "32",
    }
}

impl RustTarget {
    /// The target package analysis uses unless the caller selects another one.
    ///
    /// It is `x86_64-unknown-linux-gnu` on every host, so one package release gives the
    /// same facts wherever it is analyzed.
    pub const PACKAGE: Self = TARGETS[0];

    /// The target `triple` names, or `None` for a triple outside the supported targets.
    #[must_use]
    pub fn from_triple(triple: &str) -> Option<Self> {
        TARGETS.into_iter().find(|target| target.triple == triple)
    }

    /// The target this process was built for, which a local workspace uses; `None` on a
    /// host outside the supported targets.
    #[must_use]
    pub fn host() -> Option<Self> {
        let env = if cfg!(target_env = "gnu") {
            "gnu"
        } else if cfg!(target_env = "musl") {
            "musl"
        } else if cfg!(target_env = "msvc") {
            "msvc"
        } else {
            ""
        };
        // Every supported target is little-endian.
        let pointer_width = if cfg!(target_pointer_width = "64") {
            "64"
        } else if cfg!(target_pointer_width = "32") {
            "32"
        } else {
            return None;
        };
        TARGETS.into_iter().find(|target| {
            cfg!(target_endian = "little")
                && target.pointer_width == pointer_width
                && target.arch == std::env::consts::ARCH
                && target.os == std::env::consts::OS
                && target.env == env
        })
    }

    /// The target triple.
    #[must_use]
    pub const fn triple(self) -> &'static str {
        self.triple
    }

    /// The value of one configuration predicate, `None` for a predicate no target decides.
    pub(crate) fn predicate(self, atom: &[&str]) -> Option<bool> {
        let (name, value) = match atom {
            ["unix"] => return Some(self.family == "unix"),
            ["windows"] => return Some(self.family == "windows"),
            [name, "=", value] => (*name, value.strip_prefix('"')?.strip_suffix('"')?),
            _ => return None,
        };
        let established = match name {
            "target_family" => self.family,
            "target_os" => self.os,
            "target_arch" => self.arch,
            "target_pointer_width" => self.pointer_width,
            "target_endian" => "little",
            "target_env" => self.env,
            "target_vendor" => self.vendor,
            _ => return None,
        };
        Some(established == value)
    }
}

#[cfg(test)]
mod tests {
    use super::{RustTarget, TARGETS};

    fn known(target: RustTarget, name: &str, value: &str) -> bool {
        let value = format!("\"{value}\"");
        target
            .predicate(&[name, "=", value.as_str()])
            .expect("target predicate")
    }

    #[test]
    fn test_targets_have_distinct_triples_and_the_package_target_is_fixed() {
        for (index, target) in TARGETS.iter().enumerate() {
            assert_eq!(RustTarget::from_triple(target.triple()), Some(*target));
            assert!(
                TARGETS[..index]
                    .iter()
                    .all(|other| other.triple != target.triple)
            );
        }
        assert_eq!(RustTarget::from_triple("unknown"), None);
        assert_eq!(RustTarget::PACKAGE.triple(), "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn test_targets_decide_their_own_predicates_only() {
        let linux = RustTarget::PACKAGE;
        let windows = RustTarget::from_triple("aarch64-pc-windows-msvc").expect("windows");
        let macos = RustTarget::from_triple("aarch64-apple-darwin").expect("macos");
        let wasm = RustTarget::from_triple("wasm32-wasip1").expect("wasm");
        for (target, unix, windows) in [
            (linux, true, false),
            (windows, false, true),
            (macos, true, false),
            (wasm, false, false),
        ] {
            assert_eq!(target.predicate(&["unix"]), Some(unix));
            assert_eq!(target.predicate(&["windows"]), Some(windows));
        }
        for (target, os, arch, env, vendor, family, width) in [
            (linux, "linux", "x86_64", "gnu", "unknown", "unix", "64"),
            (windows, "windows", "aarch64", "msvc", "pc", "windows", "64"),
            (macos, "macos", "aarch64", "", "apple", "unix", "64"),
            (wasm, "wasi", "wasm32", "p1", "unknown", "wasm", "32"),
        ] {
            assert!(known(target, "target_os", os));
            assert!(known(target, "target_arch", arch));
            assert!(known(target, "target_env", env));
            assert!(known(target, "target_vendor", vendor));
            assert!(known(target, "target_family", family));
            assert!(known(target, "target_pointer_width", width));
            assert!(known(target, "target_endian", "little"));
            assert!(!known(target, "target_endian", "big"));
            assert!(!known(target, "target_os", "emscripten"));
        }
        for unknown in [
            &["custom"][..],
            &["test"],
            &["debug_assertions"],
            &["feature", "=", "\"std\""],
            &["target_feature", "=", "\"sse2\""],
            &["target_has_atomic", "=", "\"64\""],
            &["panic", "=", "\"unwind\""],
            &["target_os", "=", "linux"],
        ] {
            assert_eq!(linux.predicate(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn test_host_target_agrees_with_this_build() {
        let host = RustTarget::host().expect("a supported development host");
        assert_eq!(host.predicate(&["unix"]), Some(cfg!(unix)));
        assert_eq!(host.predicate(&["windows"]), Some(cfg!(windows)));
        assert!(known(host, "target_family", std::env::consts::FAMILY));
        assert!(known(host, "target_os", std::env::consts::OS));
        assert!(known(host, "target_arch", std::env::consts::ARCH));
        assert_eq!(
            known(host, "target_pointer_width", "64"),
            cfg!(target_pointer_width = "64")
        );
        assert_eq!(
            known(host, "target_vendor", "apple"),
            cfg!(target_vendor = "apple")
        );
        assert_eq!(
            known(host, "target_endian", "little"),
            cfg!(target_endian = "little")
        );
    }
}
