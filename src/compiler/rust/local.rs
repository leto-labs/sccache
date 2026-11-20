use super::{
    ARGS, ArgCodegen, ArgData, ArgTarget, COMMAND_LINE_LIMIT, arguments_for_spawn,
    command_line_len, extra_filename_from_args, metadata_filename,
};
use crate::compiler::args::{ArgDisposition, ArgsIter, Argument, FromArg};
use crate::compiler::is_rustc_like;
use crate::config::{RustIncremental, RustIncrementalFirstBuild, RustIncrementalSettings};
use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub(crate) fn plain_first_build_arguments(
    executable: &Path,
    arguments: &[OsString],
    cwd: &Path,
    settings: RustIncrementalSettings,
) -> Option<(Vec<OsString>, Option<tempfile::NamedTempFile>)> {
    if settings.mode == RustIncremental::Off
        || settings.first_build != RustIncrementalFirstBuild::Plain
        || !is_rustc_like(executable)
        || !arguments.iter().any(|arg| {
            arg.as_encoded_bytes()
                .windows(b"incremental=".len())
                .any(|part| part == b"incremental=")
                || arg.as_encoded_bytes().starts_with(b"@")
        })
    {
        return None;
    }

    let mut raw = expand_args(cwd, arguments)?;
    let consumed = Cell::new(0);
    let mut args = ArgsIter::new(
        raw.iter()
            .cloned()
            .inspect(|_| consumed.set(consumed.get() + 1)),
        &ARGS[..],
    )
    .with_double_dashes();
    let mut parsed = Vec::new();
    let mut incremental = Vec::new();
    let mut explicit_lto = false;
    let mut start = 0;
    while let Some(arg) = args.next() {
        let mut arg = arg.ok()?;
        let mut grouped_flags = None;
        if let Argument::UnknownFlag(flag) = &arg
            && let Some((flags, value)) = grouped_codegen(flag)
        {
            let value = if value.is_empty() {
                let Argument::Raw(value) = args.next()?.ok()? else {
                    return None;
                };
                value
            } else {
                value.into()
            };
            grouped_flags = Some(OsString::from(flags));
            arg = Argument::WithValue(
                "-C",
                ArgData::CodeGen(ArgCodegen::process(value).ok()?),
                ArgDisposition::Separated,
            );
        }
        if matches!(&arg, Argument::Raw(value) if value == "--") {
            break;
        }
        if let Some(ArgData::CodeGen(ArgCodegen { opt, value })) = arg.get_data() {
            match opt.as_str() {
                "incremental" => {
                    value.as_ref()?;
                    incremental.push((start..consumed.get(), grouped_flags));
                }
                "lto" => explicit_lto = true,
                _ => (),
            }
        }
        parsed.push(arg);
        start = consumed.get();
    }

    if incremental.is_empty() {
        return None;
    }
    let output = cwd.join(primary_output(&parsed)?);
    if output.try_exists().ok()? {
        return None;
    }

    if !explicit_lto {
        raw.splice(
            start..start,
            [OsString::from("-C"), OsString::from("lto=off")],
        );
    }
    for (range, flags) in incremental.into_iter().rev() {
        raw.splice(range, flags);
    }
    let (args, response_file) = arguments_for_spawn(executable, &raw, COMMAND_LINE_LIMIT).ok()?;
    if COMMAND_LINE_LIMIT.is_some_and(|limit| command_line_len(executable, &args) > limit) {
        return None;
    }
    Some((args, response_file))
}

fn expand_args(cwd: &Path, arguments: &[OsString]) -> Option<Vec<OsString>> {
    let mut expanded = Vec::new();
    for arg in arguments {
        if let Some(path) = arg.to_str()?.strip_prefix('@') {
            if path.starts_with("shell:") {
                return None;
            }
            let contents = fs_err::read_to_string(cwd.join(path)).ok()?;
            // Rustc reads these lines verbatim. Passing a literal @ back on
            // the command line would expand it, unlike inside the file.
            if contents.lines().any(|line| line.starts_with('@')) {
                return None;
            }
            expanded.extend(contents.lines().map(OsString::from));
        } else {
            expanded.push(arg.clone());
        }
    }
    Some(expanded)
}

fn grouped_codegen(flag: &OsStr) -> Option<(&str, &str)> {
    let flag = flag.to_str()?;
    let short = flag.strip_prefix('-')?;
    let rest = short.trim_start_matches(['g', 'O', 'v', 'V', 'h']);
    if rest == short {
        return None;
    }
    Some((&flag[..flag.len() - rest.len()], rest.strip_prefix('C')?))
}

fn primary_output(arguments: &[Argument<ArgData>]) -> Option<PathBuf> {
    let mut crate_name = None;
    let mut crate_type = None;
    let mut out_dir = None;
    let mut target = None;
    let mut has_emit = false;
    let mut emit_link = false;
    let mut emit_metadata = false;
    let mut is_test = false;
    for arg in arguments {
        match arg.get_data() {
            Some(ArgData::CrateName(name)) => crate_name = Some(name),
            Some(ArgData::CrateType(types)) if crate_type.is_none() => crate_type = Some(types),
            Some(ArgData::CrateType(_)) => return None,
            Some(ArgData::OutDir(dir)) => out_dir = Some(dir),
            Some(ArgData::Target(value)) => target = Some(value),
            Some(ArgData::Emit(value)) => {
                has_emit = true;
                for emit in value.split(',') {
                    match emit {
                        "link" => emit_link = true,
                        "metadata" => emit_metadata = true,
                        _ if emit.starts_with("link=") || emit.starts_with("metadata=") => {
                            return None;
                        }
                        _ => (),
                    }
                }
            }
            Some(ArgData::NotCompilationFlag | ArgData::NotCompilation(_)) => return None,
            Some(ArgData::TooHardPath(_)) if arg.flag_str() == Some("-o") => return None,
            _ => is_test |= matches!(arg, Argument::UnknownFlag(flag) if flag == "--test"),
        }
    }

    let crate_name = crate_name?;
    let out_dir = out_dir?;
    if has_emit && !emit_link {
        return emit_metadata.then(|| out_dir.join(metadata_filename(crate_name, arguments)));
    }
    let (exe_suffix, dylib) = target_suffixes(target)?;
    let (prefix, suffix) = match (is_test, crate_type) {
        (true, _) | (_, None) => ("", exe_suffix),
        (false, Some(types)) if !types.rlib && !types.staticlib && types.others.len() == 1 => {
            match types.others.iter().next()?.as_str() {
                "bin" => ("", exe_suffix),
                "proc-macro" => dylib?,
                _ => return None,
            }
        }
        _ => return None,
    };
    let extra = extra_filename_from_args(arguments);
    Some(out_dir.join(format!("{prefix}{crate_name}{extra}{suffix}")))
}

fn target_suffixes(target: Option<&ArgTarget>) -> Option<(&str, Option<(&str, &str)>)> {
    use std::env::consts::{DLL_PREFIX, DLL_SUFFIX, EXE_SUFFIX};
    let target = match target {
        None => return Some((EXE_SUFFIX, Some((DLL_PREFIX, DLL_SUFFIX)))),
        Some(ArgTarget::Name(name)) => name,
        _ => return None,
    };
    let parts: Vec<_> = target.split('-').collect();
    Some(match parts.as_slice() {
        [_, _, "windows", ..] => (".exe", Some(("", ".dll"))),
        [
            _,
            "apple",
            "darwin" | "ios" | "tvos" | "watchos" | "visionos",
            ..,
        ] => ("", Some(("lib", ".dylib"))),
        [
            _,
            _,
            "linux" | "freebsd" | "netbsd" | "openbsd" | "dragonfly",
            ..,
        ]
        | [_, "linux", "android" | "androideabi"] => ("", Some(("lib", ".so"))),
        ["wasm32", "unknown", "emscripten"] => (".js", None),
        ["wasm32" | "wasm64", "unknown", "unknown"]
        | ["wasm32", "wasip1" | "wasip2" | "wasip3", ..] => (".wasm", None),
        [_, "unknown", "uefi"] => (".efi", None),
        _ => return None,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use std::env::consts::{DLL_PREFIX, DLL_SUFFIX, EXE_SUFFIX};
    use test_case::test_case;

    fn rustc_args(extra: &[&str]) -> Vec<OsString> {
        [
            "src.rs",
            "--crate-name=demo",
            "--out-dir",
            "out",
            "-Cextra-filename=-hash",
        ]
        .into_iter()
        .chain(extra.iter().copied())
        .map(OsString::from)
        .collect()
    }

    #[test_case(&["--test", "--emit=dep-info,link"], Some(&format!("demo-hash{EXE_SUFFIX}")); "test harness")]
    #[test_case(&["--crate-type=bin", "--emit", "link"], Some(&format!("demo-hash{EXE_SUFFIX}")); "bin")]
    #[test_case(&["--emit=dep-info,link"], Some(&format!("demo-hash{EXE_SUFFIX}")); "test with harness disabled")]
    #[test_case(&["--crate-type=proc-macro"], Some(&format!("{DLL_PREFIX}demo-hash{DLL_SUFFIX}")); "proc macro")]
    #[test_case(&["--test", "--emit=dep-info,metadata", "-C", "extra_filename=-last"], Some("libdemo-last.rmeta"); "metadata only")]
    #[test_case(&["--target=x86_64-pc-windows-msvc", "--test"], Some("demo-hash.exe"); "windows test harness")]
    #[test_case(&["--target=wasm32-wasip1", "--crate-type=bin"], Some("demo-hash.wasm"); "webassembly bin")]
    #[test_case(&["--target=custom.json", "--crate-type=bin"], None; "custom target")]
    #[test_case(&["--emit=link=elsewhere"], None; "explicit emit path")]
    #[test_case(&["--emit=dep-info=elsewhere,metadata"], Some("libdemo-hash.rmeta"); "explicit dep-info path")]
    #[test_case(&["-o", "elsewhere"], None; "explicit output path")]
    fn test_primary_output(extra: &[&str], expected: Option<&str>) {
        let args = ArgsIter::new(rustc_args(extra).into_iter(), &ARGS[..])
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            primary_output(&args),
            expected.map(|name| Path::new("out").join(name))
        );
    }

    #[test_case(&["-C", "incremental=inc"], &[]; "short separated")]
    #[test_case(&["-Cincremental=inc"], &[]; "short joined")]
    #[test_case(&["--codegen", "incremental=inc"], &[]; "long separated")]
    #[test_case(&["--codegen=incremental=inc"], &[]; "long joined")]
    #[test_case(&["-gOCincremental=inc"], &["-gO"]; "grouped joined")]
    #[test_case(&["-gC", "incremental=inc"], &["-g"]; "grouped separated")]
    fn test_plain_first_build_incremental_spellings(incremental: &[&str], remaining: &[&str]) {
        let temp = tempfile::tempdir().unwrap();
        let args = rustc_args(incremental);
        let settings = RustIncrementalSettings {
            mode: RustIncremental::Cache,
            first_build: RustIncrementalFirstBuild::Plain,
        };
        let (plain, _) =
            plain_first_build_arguments(Path::new("rustc"), &args, temp.path(), settings).unwrap();
        assert_eq!(plain, rustc_args(&[remaining, &["-C", "lto=off"]].concat()));
    }

    #[test_case(RustIncremental::Cache, RustIncrementalFirstBuild::Plain, false, &[], Some(&["-C", "lto=off"]); "missing output")]
    #[test_case(RustIncremental::CacheNoBypass, RustIncrementalFirstBuild::Plain, false, &[], Some(&["-C", "lto=off"]); "cache no bypass")]
    #[test_case(RustIncremental::Cache, RustIncrementalFirstBuild::Plain, true, &[], None; "existing output")]
    #[test_case(RustIncremental::Off, RustIncrementalFirstBuild::Plain, false, &[], None; "mode off")]
    #[test_case(RustIncremental::Cache, RustIncrementalFirstBuild::Incremental, false, &[], None; "incremental first build")]
    #[test_case(RustIncremental::Cache, RustIncrementalFirstBuild::Plain, false, &["--codegen=lto=thin"], Some(&["--codegen=lto=thin"]); "explicit lto")]
    #[test_case(RustIncremental::Cache, RustIncrementalFirstBuild::Plain, false, &["-gClto"], Some(&["-gClto"]); "grouped explicit lto")]
    #[test_case(RustIncremental::Cache, RustIncrementalFirstBuild::Plain, false, &["--emit=dep-info"], None; "unknown primary output")]
    fn test_plain_first_build_decision(
        mode: RustIncremental,
        first_build: RustIncrementalFirstBuild,
        has_output: bool,
        extra: &[&str],
        expected: Option<&[&str]>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        if has_output {
            let out = temp.path().join("out");
            std::fs::create_dir(&out).unwrap();
            std::fs::write(out.join(format!("demo-hash{EXE_SUFFIX}")), "").unwrap();
        }
        let args = rustc_args(&[&["--codegen=incremental=inc"][..], extra].concat());
        let settings = RustIncrementalSettings { mode, first_build };
        let plain = plain_first_build_arguments(Path::new("rustc"), &args, temp.path(), settings);
        assert_eq!(plain.map(|(args, _)| args), expected.map(rustc_args));
    }

    #[test]
    fn test_plain_first_build_ignores_non_incremental_commands() {
        let temp = tempfile::tempdir().unwrap();
        let settings = RustIncrementalSettings {
            mode: RustIncremental::Cache,
            first_build: RustIncrementalFirstBuild::Plain,
        };
        for (exe, args) in [
            ("rustc", &[][..]),
            ("rustc", &["--cfg", "incremental=\"cfg\""]),
            ("rustc", &["--", "-Cincremental=source.rs"]),
            ("clang", &["-Cincremental=inc"]),
        ] {
            let args = rustc_args(args);
            assert!(
                plain_first_build_arguments(Path::new(exe), &args, temp.path(), settings).is_none()
            );
        }
    }

    #[test]
    fn test_plain_first_build_response_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("args"),
            "--crate-name=demo\n--out-dir\nout\n-Cincremental=inc\n-C\nlink-arg=trailing space \n",
        )
        .unwrap();
        let settings = RustIncrementalSettings {
            mode: RustIncremental::Cache,
            first_build: RustIncrementalFirstBuild::Plain,
        };
        let args = vec!["@args".into(), "src.rs".into()];
        let (plain, _) =
            plain_first_build_arguments(Path::new("rustc"), &args, temp.path(), settings).unwrap();
        assert_eq!(
            plain,
            [
                "--crate-name=demo",
                "--out-dir",
                "out",
                "-C",
                "link-arg=trailing space ",
                "src.rs",
                "-C",
                "lto=off"
            ]
            .map(OsString::from)
        );
    }
}
