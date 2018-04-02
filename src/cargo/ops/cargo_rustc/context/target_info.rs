use std::cell::RefCell;
use std::collections::hash_map::{Entry, HashMap};
use std::path::PathBuf;
use std::str::{self, FromStr};

use super::{env_args, Context};
use util::{CargoResult, CargoResultExt, Cfg, ProcessBuilder};
use core::TargetKind;
use ops::Kind;

#[derive(Clone, Default)]
pub struct TargetInfo {
    crate_type_process: Option<ProcessBuilder>,
    crate_types: RefCell<HashMap<String, Option<(String, String)>>>,
    cfg: Option<Vec<Cfg>>,
    pub sysroot_libdir: Option<PathBuf>,
}

/// Type of each file generated by a Unit.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TargetFileType {
    /// Not a special file type.
    Normal,
    /// It is something you can link against (e.g. a library)
    Linkable,
    /// It is a piece of external debug information (e.g. *.dSYM and *.pdb)
    DebugInfo,
}

pub struct FileType {
    pub suffix: String,
    pub prefix: String,
    pub target_file_type: TargetFileType,
    pub should_replace_hyphens: bool,
}

impl TargetInfo {
    pub fn new(cx: &Context, kind: Kind) -> CargoResult<TargetInfo> {
        let rustflags = env_args(cx.config, &cx.build_config, None, kind, "RUSTFLAGS")?;
        let mut process = cx.config.rustc()?.process();
        process
            .arg("-")
            .arg("--crate-name")
            .arg("___")
            .arg("--print=file-names")
            .args(&rustflags)
            .env_remove("RUST_LOG");

        if kind == Kind::Target {
            process.arg("--target").arg(&cx.target_triple());
        }

        let crate_type_process = process.clone();
        const KNOWN_CRATE_TYPES: &[&str] =
            &["bin", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
        for crate_type in KNOWN_CRATE_TYPES.iter() {
            process.arg("--crate-type").arg(crate_type);
        }

        let mut with_cfg = process.clone();
        with_cfg.arg("--print=sysroot");
        with_cfg.arg("--print=cfg");

        let mut has_cfg_and_sysroot = true;
        let output = with_cfg
            .exec_with_output()
            .or_else(|_| {
                has_cfg_and_sysroot = false;
                process.exec_with_output()
            })
            .chain_err(|| "failed to run `rustc` to learn about target-specific information")?;

        let error = str::from_utf8(&output.stderr).unwrap();
        let output = str::from_utf8(&output.stdout).unwrap();
        let mut lines = output.lines();
        let mut map = HashMap::new();
        for crate_type in KNOWN_CRATE_TYPES {
            let out = parse_crate_type(crate_type, error, &mut lines)?;
            map.insert(crate_type.to_string(), out);
        }

        let mut sysroot_libdir = None;
        if has_cfg_and_sysroot {
            let line = match lines.next() {
                Some(line) => line,
                None => bail!(
                    "output of --print=sysroot missing when learning about \
                     target-specific information from rustc"
                ),
            };
            let mut rustlib = PathBuf::from(line);
            if kind == Kind::Host {
                if cfg!(windows) {
                    rustlib.push("bin");
                } else {
                    rustlib.push("lib");
                }
                sysroot_libdir = Some(rustlib);
            } else {
                rustlib.push("lib");
                rustlib.push("rustlib");
                rustlib.push(cx.target_triple());
                rustlib.push("lib");
                sysroot_libdir = Some(rustlib);
            }
        }

        let cfg = if has_cfg_and_sysroot {
            Some(lines.map(Cfg::from_str).collect::<CargoResult<_>>()?)
        } else {
            None
        };

        Ok(TargetInfo {
            crate_type_process: Some(crate_type_process),
            crate_types: RefCell::new(map),
            cfg,
            sysroot_libdir,
        })
    }

    pub fn cfg(&self) -> Option<&[Cfg]> {
        self.cfg.as_ref().map(|v| v.as_ref())
    }

    pub fn file_types(
        &self,
        crate_type: &str,
        file_type: TargetFileType,
        kind: &TargetKind,
        target_triple: &str,
    ) -> CargoResult<Option<Vec<FileType>>> {
        let mut crate_types = self.crate_types.borrow_mut();
        let entry = crate_types.entry(crate_type.to_string());
        let crate_type_info = match entry {
            Entry::Occupied(o) => &*o.into_mut(),
            Entry::Vacant(v) => {
                let value = self.discover_crate_type(v.key())?;
                &*v.insert(value)
            }
        };
        let (prefix, suffix) = match *crate_type_info {
            Some((ref prefix, ref suffix)) => (prefix, suffix),
            None => return Ok(None),
        };
        let mut ret = vec![
            FileType {
                suffix: suffix.to_string(),
                prefix: prefix.clone(),
                target_file_type: file_type,
                should_replace_hyphens: false,
            },
        ];

        // rust-lang/cargo#4500
        if target_triple.ends_with("pc-windows-msvc") && crate_type.ends_with("dylib")
            && suffix == ".dll"
        {
            ret.push(FileType {
                suffix: ".dll.lib".to_string(),
                prefix: prefix.clone(),
                target_file_type: TargetFileType::Normal,
                should_replace_hyphens: false,
            })
        }

        // rust-lang/cargo#4535
        if target_triple.starts_with("wasm32-") && crate_type == "bin" && suffix == ".js" {
            ret.push(FileType {
                suffix: ".wasm".to_string(),
                prefix: prefix.clone(),
                target_file_type: TargetFileType::Normal,
                should_replace_hyphens: true,
            })
        }

        // rust-lang/cargo#4490, rust-lang/cargo#4960
        //  - only uplift debuginfo for binaries.
        //    tests are run directly from target/debug/deps/
        //    and examples are inside target/debug/examples/ which already have symbols next to them
        //    so no need to do anything.
        if *kind == TargetKind::Bin {
            if target_triple.contains("-apple-") {
                ret.push(FileType {
                    suffix: ".dSYM".to_string(),
                    prefix: prefix.clone(),
                    target_file_type: TargetFileType::DebugInfo,
                    should_replace_hyphens: false,
                })
            } else if target_triple.ends_with("-msvc") {
                ret.push(FileType {
                    suffix: ".pdb".to_string(),
                    prefix: prefix.clone(),
                    target_file_type: TargetFileType::DebugInfo,
                    should_replace_hyphens: false,
                })
            }
        }

        Ok(Some(ret))
    }

    fn discover_crate_type(&self, crate_type: &str) -> CargoResult<Option<(String, String)>> {
        let mut process = self.crate_type_process.clone().unwrap();

        process.arg("--crate-type").arg(crate_type);

        let output = process.exec_with_output().chain_err(|| {
            format!(
                "failed to run `rustc` to learn about \
                 crate-type {} information",
                crate_type
            )
        })?;

        let error = str::from_utf8(&output.stderr).unwrap();
        let output = str::from_utf8(&output.stdout).unwrap();
        Ok(parse_crate_type(crate_type, error, &mut output.lines())?)
    }
}

/// Takes rustc output (using specialized command line args), and calculates the file prefix and
/// suffix for the given crate type, or returns None if the type is not supported. (e.g. for a
/// rust library like libcargo.rlib, prefix = "lib", suffix = "rlib").
///
/// The caller needs to ensure that the lines object is at the correct line for the given crate
/// type: this is not checked.
// This function can not handle more than 1 file per type (with wasm32-unknown-emscripten, there
// are 2 files for bin (.wasm and .js))
fn parse_crate_type(
    crate_type: &str,
    error: &str,
    lines: &mut str::Lines,
) -> CargoResult<Option<(String, String)>> {
    let not_supported = error.lines().any(|line| {
        (line.contains("unsupported crate type") || line.contains("unknown crate type"))
            && line.contains(crate_type)
    });
    if not_supported {
        return Ok(None);
    }
    let line = match lines.next() {
        Some(line) => line,
        None => bail!(
            "malformed output when learning about \
             crate-type {} information",
            crate_type
        ),
    };
    let mut parts = line.trim().split("___");
    let prefix = parts.next().unwrap();
    let suffix = match parts.next() {
        Some(part) => part,
        None => bail!(
            "output of --print=file-names has changed in \
             the compiler, cannot parse"
        ),
    };

    Ok(Some((prefix.to_string(), suffix.to_string())))
}
