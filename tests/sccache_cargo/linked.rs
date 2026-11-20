// Any copyright is dedicated to the Public Domain.
// http://creativecommons.org/publicdomain/zero/1.0/

use super::{
    CARGO, SccacheTest, incremental_crate_dirs, incremental_stats, restart_sccache,
    zero_incremental_stats,
};
use anyhow::{Context, Result, bail};
use assert_cmd::prelude::*;
use fs_err as fs;
use predicates::prelude::*;
use serde_json::Value;
use serial_test::serial;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use test_case::test_case;

const NATIVE_TEST: &str = r#"
#[test]
fn native() {
    println!("native value: {}", mid::value());
    println!("fixture: {}", include_str!("../../outside.txt").trim());
}
"#;

#[derive(Clone, Copy)]
struct Linker {
    driver: &'static str,
    fuse_ld: Option<&'static str>,
}

impl Linker {
    fn probe(name: &str) -> Result<Option<Self>> {
        let (driver, fuse_ld) = match name {
            "rust-lld" => ("cc", None),
            "mold" => ("clang", Some("-Clink-arg=-fuse-ld=mold")),
            "bfd" => ("cc", Some("-Clink-arg=-fuse-ld=bfd")),
            _ => unreachable!(),
        };
        let probe = tempfile::tempdir()?;
        fs::write(probe.path().join("probe.rs"), "#[test] fn smoke() {}")?;
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .current_dir(probe.path())
            .args([
                "--test",
                "probe.rs",
                "--emit=link",
                "--print",
                "link-args",
                "-Clink-arg=-Wl,--dependency-file=link.d",
            ])
            .arg(format!("-Clinker={driver}"))
            .args(fuse_ld)
            .output()?;
        if !output.status.success()
            || !probe.path().join("link.d").is_file()
            || (name == "rust-lld" && !String::from_utf8_lossy(&output.stdout).contains("/gcc-ld"))
        {
            eprintln!(
                "skipping {name}: usable GNU dependency file unavailable\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(None);
        }
        Ok(Some(Self { driver, fuse_ld }))
    }
}

struct Fixture {
    test: SccacheTest<'static>,
    a: PathBuf,
    b: PathBuf,
    wrapper: PathBuf,
    miss_log: PathBuf,
    linker: Linker,
}

impl Fixture {
    fn new(linker: Linker, stage: &str, extra_env: &[(&str, &str)]) -> Result<Self> {
        let test = SccacheTest::new(None)?;
        let root = test.tempdir.path();
        let fixture = Self {
            a: root.join("checkout-a"),
            b: root.join("checkout-b"),
            wrapper: root.join("linker"),
            miss_log: root.join("misses.jsonl"),
            test,
            linker,
        };
        fixture.write_wrapper("# initial wrapper")?;
        restart_sccache(
            &fixture.test,
            Some(
                [
                    (
                        "SCCACHE_MISS_EXPLAIN",
                        fixture.miss_log.to_str().context("non-UTF-8 miss log")?,
                    ),
                    ("SCCACHE_RUST_CACHE_LINKED", stage),
                ]
                .into_iter()
                .chain(extra_env.iter().copied())
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect(),
            ),
        )?;
        Ok(fixture)
    }

    fn write_wrapper(&self, setup: &str) -> Result<()> {
        fs::write(
            &self.wrapper,
            format!("#!/bin/sh\n{setup}\nexec {} \"$@\"\n", self.linker.driver),
        )?;
        fs::set_permissions(&self.wrapper, Permissions::from_mode(0o755))?;
        Ok(())
    }

    fn cargo_with_flags(
        &self,
        checkout: &Path,
        extra_flags: impl IntoIterator<Item = OsString>,
    ) -> Command {
        let mut linker = OsString::from("-Clinker=");
        linker.push(&self.wrapper);
        let mut flags = vec![linker];
        flags.extend(self.linker.fuse_ld.map(OsString::from));
        flags.extend(extra_flags);
        let mut cmd = Command::new(CARGO.as_os_str());
        cmd.envs(self.test.env.iter().cloned())
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env_remove("RUSTFLAGS")
            .env_remove("LIBRARY_PATH")
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("SCCACHE_CLIENT_SIDE")
            .env_remove("SCCACHE_RECACHE")
            .env_remove("SCCACHE_NO_CACHE")
            .env("SCCACHE_BASEDIRS", checkout)
            .env("SCCACHE_BASEDIRS_REMAP_PATHS", "1")
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                flags.join(std::ffi::OsStr::new("\x1f")),
            )
            .current_dir(checkout);
        cmd
    }

    fn cargo(&self, checkout: &Path) -> Command {
        self.cargo_with_flags(checkout, [])
    }

    fn miss_keys(&self, crate_name: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for line in fs::read_to_string(&self.miss_log)?.lines() {
            let miss: Value = serde_json::from_str(line)?;
            if miss["crate_name"] == crate_name {
                keys.push(
                    miss["key"]
                        .as_str()
                        .context("missing cache key")?
                        .to_owned(),
                );
            }
        }
        Ok(keys)
    }
}

fn test_artifacts(cargo: &mut Command) -> Result<BTreeMap<String, PathBuf>> {
    let output = cargo.arg("--message-format=json").output()?;
    output.clone().assert().try_success()?;
    let mut artifacts = BTreeMap::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let artifact: Value = serde_json::from_str(line)?;
        if artifact["reason"] == "compiler-artifact"
            && artifact["profile"]["test"] == true
            && let Some(path) = artifact["executable"].as_str()
        {
            let name = artifact["target"]["name"]
                .as_str()
                .context("unnamed target")?;
            artifacts.insert(name.to_owned(), path.into());
        }
    }
    Ok(artifacts)
}

pub(super) fn write_package(
    root: &Path,
    name: &str,
    manifest: &str,
    sources: &[(&str, &str)],
) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname = '{name}'\nversion = '0.1.0'\nedition = '2021'\n{manifest}\n"),
    )?;
    for (path, source) in sources {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, source)?;
    }
    Ok(())
}

fn write_native_workspace(root: &Path) -> Result<()> {
    super::write_linked_native_crates(root)?;
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = ['nat', 'mid', 'app']\nresolver = '2'\n",
    )?;
    write_package(
        &root.join("app"),
        "harness-app",
        "[dependencies]\nmid = { path = '../mid' }",
        &[
            ("src/lib.rs", "#[test] fn unit() { assert_eq!(2 + 2, 4); }"),
            ("tests/shared.rs", NATIVE_TEST),
        ],
    )?;
    fs::write(root.join("outside.txt"), "original\n")?;
    fs::create_dir(root.join("earlier"))?;
    Ok(())
}

fn run_harness(path: &Path, expected: &str) -> Result<()> {
    Command::new(path)
        .arg("--nocapture")
        .assert()
        .try_success()?
        .try_stdout(predicate::str::contains(expected))?;
    Ok(())
}

#[test_case("rust-lld")]
#[test_case("mold")]
#[test_case("bfd")]
#[serial]
fn test_rust_cargo_linked_harnesses(linker_name: &str) -> Result<()> {
    let Some(linker) = Linker::probe(linker_name)? else {
        return Ok(());
    };
    let fixture = Fixture::new(linker, "tests", &[])?;
    let Fixture { test, a, b, .. } = &fixture;
    for checkout in [a, b] {
        write_native_workspace(checkout)?;
    }
    let cargo_tests = |checkout: &Path| {
        let mut cmd = fixture.cargo(checkout);
        cmd.args([
            "test",
            "--offline",
            "-p",
            "harness-app",
            "--lib",
            "--test",
            "shared",
        ]);
        cmd
    };
    let build_all = |checkout| test_artifacts(cargo_tests(checkout).arg("--no-run"));
    let artifacts_a = build_all(a)?;
    assert_eq!(artifacts_a.len(), 2);
    incremental_stats(test, 0, 5)?;
    zero_incremental_stats()?;
    let start = Instant::now();
    let artifacts_b = build_all(b)?;
    let elapsed = start.elapsed();
    let stats = incremental_stats(test, 5, 0)?;
    let executable_a = &artifacts_a["shared"];
    let executable_b = &artifacts_b["shared"];
    let original = fs::read(executable_a)?;
    assert_eq!(original, fs::read(executable_b)?);
    let mode = fs::metadata(executable_a)?.mode() & 0o777;
    assert_ne!(mode & 0o111, 0);
    assert_eq!(mode, fs::metadata(executable_b)?.mode() & 0o777);
    let key = &fixture.miss_keys("shared")?[0];
    let entry = test
        .tempdir
        .path()
        .join("cache")
        .join(&key[..1])
        .join(&key[1..2])
        .join(key);
    eprintln!(
        "{linker_name}: harness {} bytes, cache entry {} bytes, B cargo build with five hits {elapsed:?}, cache read time {:?}",
        original.len(),
        fs::metadata(entry)?.len(),
        stats["cache_read_hit_duration"]
    );
    cargo_tests(b)
        .args(["--", "--nocapture"])
        .assert()
        .try_success()?
        .try_stdout(predicate::str::contains("native value: 42"))?;

    let build = |checkout| {
        let mut cmd = fixture.cargo(checkout);
        cmd.args([
            "test",
            "--offline",
            "-p",
            "harness-app",
            "--test",
            "shared",
            "--no-run",
        ]);
        cmd
    };

    fs::remove_file(executable_b)?;
    test_artifacts(build(b).env("SCCACHE_NO_CACHE", "1"))?;
    assert_eq!(
        original,
        fs::read(executable_b)?,
        "{linker_name} cold rebuild"
    );

    let force_build = || -> Result<()> {
        fs::remove_file(executable_b)?;
        zero_incremental_stats()
    };
    fs::write(a.join("nat/answer.c"), "int answer(void) { return 43; }\n")?;
    zero_incremental_stats()?;
    test_artifacts(&mut build(a))?;
    let misses = fixture.miss_keys("shared")?;
    assert_eq!(misses.len(), 2);
    assert_eq!(misses[0], misses[1], "native input was transitive");
    run_harness(executable_a, "native value: 43")?;

    fixture.write_wrapper("# changed wrapper bytes")?;
    force_build()?;
    test_artifacts(&mut build(b))?;
    incremental_stats(test, 0, 1)?;
    assert_ne!(fixture.miss_keys("shared")?[2], misses[0]);

    force_build()?;
    fixture
        .cargo(b)
        .args([
            "rustc",
            "--offline",
            "-p",
            "harness-app",
            "--test",
            "shared",
            "--",
            "-Clink-arg=-Wl,--build-id=none",
        ])
        .assert()
        .try_success()?;
    incremental_stats(test, 0, 1)?;
    let executable_b = &test_artifacts(&mut build(b))?["shared"];
    let wrapper_key = fixture.miss_keys("shared")?[2].clone();
    assert_ne!(fixture.miss_keys("shared")?[3], wrapper_key);

    let force_build = || -> Result<()> {
        fs::remove_file(executable_b)?;
        zero_incremental_stats()
    };
    force_build()?;
    let start = Instant::now();
    test_artifacts(&mut build(b))?;
    let elapsed = start.elapsed();
    let stats = incremental_stats(test, 1, 0)?;
    eprintln!(
        "{linker_name}: one warm harness restore, cargo wall time {elapsed:?}, cache read time {:?}",
        stats["cache_read_hit_duration"]
    );

    Command::new("ar")
        .arg("crs")
        .arg(b.join("earlier/libalternate.a"))
        .assert()
        .try_success()?;
    force_build()?;
    test_artifacts(&mut build(b))?;
    incremental_stats(test, 0, 1)?;
    assert_eq!(fixture.miss_keys("shared")?[4], wrapper_key);

    force_build()?;
    test_artifacts(build(b).env("LIBRARY_PATH", b.join("earlier")))?;
    incremental_stats(test, 0, 1)?;
    assert_ne!(fixture.miss_keys("shared")?[5], wrapper_key);

    fs::write(b.join("outside.txt"), "changed outside the crate\n")?;
    force_build()?;
    test_artifacts(&mut build(b))?;
    incremental_stats(test, 0, 1)?;
    run_harness(executable_b, "fixture: changed outside the crate")?;

    fixture.write_wrapper(
        r#"for arg do
    case "$arg" in
        -Wl,--dependency-file=*) shift ;;
        *) shift; set -- "$@" "$arg" ;;
    esac
done"#,
    )?;
    for _ in 0..2 {
        force_build()?;
        test_artifacts(&mut build(b))?;
        let stats = incremental_stats(test, 0, 0)?;
        assert_eq!(stats["non_cacheable_compilations"], 1);
        assert_eq!(stats["cache_writes"], 0);
        run_harness(executable_b, "native value: 42")?;
    }
    Ok(())
}

#[test]
#[serial]
fn test_rust_cargo_linked_harness_verbatim_paths() -> Result<()> {
    let Some(linker) = Linker::probe("rust-lld")? else {
        return Ok(());
    };
    let fixture = Fixture::new(linker, "tests", &[])?;
    let Fixture { test, a, b, .. } = &fixture;
    for (checkout, marker) in [(a, "checkout A"), (b, "checkout B")] {
        write_package(
            checkout,
            "x",
            "[workspace]",
            &[
                (
                    "src/main.rs",
                    &format!("fn main() {{ println!(\"{marker}\"); }}"),
                ),
                ("marker", marker),
                (
                    "tests/bin_path.rs",
                    r#"
                    #[test] fn uses_local_binary() {
                        let expected = std::fs::read_to_string("marker").unwrap();
                        let bin = std::process::Command::new(env!("CARGO_BIN_EXE_x")).output().unwrap();
                        assert!(bin.status.success());
                        assert_eq!(String::from_utf8(bin.stdout).unwrap().trim(), expected);
                    }
                "#,
                ),
                (
                    "tests/manifest_path.rs",
                    r#"
                    #[test] fn uses_local_manifest_dir() {
                        assert_eq!(
                            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
                            std::env::current_dir().unwrap(),
                        );
                    }
                "#,
                ),
            ],
        )?;
    }
    let tests = |checkout| {
        let mut command = fixture.cargo(checkout);
        command.args([
            "test",
            "--offline",
            "--test",
            "bin_path",
            "--test",
            "manifest_path",
        ]);
        command
    };
    tests(a).arg("--no-run").assert().try_success()?;
    incremental_stats(test, 0, 2)?;
    zero_incremental_stats()?;
    tests(b).arg("--no-run").assert().try_success()?;
    incremental_stats(test, 0, 2)?;
    for name in ["bin_path", "manifest_path"] {
        let keys = fixture.miss_keys(name)?;
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
    }
    tests(b).assert().try_success()?;
    Ok(())
}

#[test_case("rust-lld")]
#[test_case("mold")]
#[test_case("bfd")]
#[serial]
fn test_rust_cargo_custom_harnesses(linker_name: &str) -> Result<()> {
    let Some(linker) = Linker::probe(linker_name)? else {
        return Ok(());
    };
    for stage in ["check", "proc-macro", "tests"] {
        let fixture = Fixture::new(
            linker,
            stage,
            &[("SCCACHE_RUST_INCREMENTAL", "cache-no-bypass")],
        )?;
        let Fixture { test, a, b, .. } = &fixture;
        for (checkout, value) in [(a, "11"), (b, "22")] {
            write_package(
                checkout,
                "custom-harnesses",
                r#"
                    [workspace]
                    [[test]]
                    name = "custom"
                    harness = false
                    [[test]]
                    name = "plain"
                    harness = false
                "#,
                &[
                    ("fixture.txt", value),
                    (
                        "tests/custom.rs",
                        r#"
                            fn main() {
                                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
                                let value = std::fs::read_to_string(path.join("fixture.txt")).unwrap();
                                assert_eq!(value, std::fs::read_to_string("fixture.txt").unwrap());
                                println!("custom: {value}");
                            }
                        "#,
                    ),
                    ("tests/plain.rs", r#"fn main() { println!("plain: 42"); }"#),
                ],
            )?;
        }

        let cargo = |checkout| {
            let mut command = fixture.cargo(checkout);
            command
                .env("CARGO_INCREMENTAL", "1")
                .env("SCCACHE_RUST_INCREMENTAL", "cache-no-bypass")
                .env_remove("SCCACHE_BASEDIRS_VERBATIM_ENV_DEPS");
            command
        };
        if stage == "check" {
            for (checkout, hits, misses) in [(a, 0, 2), (b, 2, 0)] {
                zero_incremental_stats()?;
                cargo(checkout)
                    .args(["check", "--offline", "--all-targets"])
                    .assert()
                    .try_success()?;
                incremental_stats(test, hits, misses)?;
            }
            let deps = Path::new("target/debug/deps");
            for name in ["custom", "plain"] {
                let prefix = format!("lib{name}-");
                let metadata = fs::read_dir(a.join(deps))?
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .find(|name| {
                        let name = name.to_string_lossy();
                        name.starts_with(&prefix) && name.ends_with(".rmeta")
                    })
                    .with_context(|| format!("missing metadata for {name}"))?;
                assert!(a.join(deps).join(&metadata).is_file());
                assert!(b.join(deps).join(&metadata).is_file());
                assert!(incremental_crate_dirs(a, name)?.is_empty());
                assert!(incremental_crate_dirs(b, name)?.is_empty());
            }
            continue;
        }

        for checkout in [a, b] {
            zero_incremental_stats()?;
            let artifacts =
                test_artifacts(cargo(checkout).args(["test", "--offline", "--no-run"]))?;
            for name in ["custom", "plain"] {
                let executable = &artifacts[name];
                assert_eq!(
                    executable.parent(),
                    Some(checkout.join("target/debug/deps").as_path())
                );
                let filename = executable
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap();
                let suffix = filename.strip_prefix(&format!("{name}-")).unwrap();
                assert!(!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
                assert_ne!(fs::metadata(executable)?.mode() & 0o111, 0);
                assert!(incremental_crate_dirs(checkout, name)?.is_empty());
            }
            if stage == "proc-macro" {
                let stats = incremental_stats(test, 0, 0)?;
                assert_eq!(stats["not_cached"]["crate-type"], 2);
                assert_eq!(stats["cache_writes"], 0);
            } else if checkout == a {
                incremental_stats(test, 0, 2)?;
            } else {
                incremental_stats(test, 1, 1)?;
            }
        }
        if stage == "tests" {
            assert_eq!(fixture.miss_keys("plain")?.len(), 1);
            let keys = fixture.miss_keys("custom")?;
            assert_eq!(keys.len(), 2);
            assert_ne!(keys[0], keys[1]);
            cargo(b)
                .args(["test", "--offline"])
                .assert()
                .try_success()?
                .try_stdout(
                    predicate::str::contains("custom: 22")
                        .and(predicate::str::contains("plain: 42")),
                )?;
        }
    }
    Ok(())
}

const SMOKE_TEST: &str = "#[test] fn smoke() { println!(\"linked harness\"); }";

fn write_smoke_package(root: &Path) -> Result<()> {
    write_package(root, "smoke", "[workspace]", &[("src/main.rs", SMOKE_TEST)])
}

fn build_smoke(fixture: &Fixture, checkout: &Path) -> Command {
    let mut cmd = fixture.cargo(checkout);
    cmd.args(["test", "--offline", "--bin", "smoke", "--no-run"]);
    cmd
}

#[test_case(false ; "compressed")]
#[test_case(true ; "reflink")]
#[serial]
fn test_rust_cargo_linked_harness_replaces_running_inode(raw: bool) -> Result<()> {
    let Some(linker) = Linker::probe("rust-lld")? else {
        return Ok(());
    };
    let fixture = Fixture::new(
        linker,
        "tests",
        &[("SCCACHE_DISK_REFLINK", if raw { "true" } else { "false" })],
    )?;
    let Fixture { test, a, b, .. } = &fixture;
    if raw {
        let root = test.tempdir.path();
        fs::write(root.join("reflink-source"), [0; 4096])?;
        let probe = Command::new("cp")
            .arg("--reflink=always")
            .arg(root.join("reflink-source"))
            .arg(root.join("reflink-copy"))
            .output()?;
        if !probe.status.success() {
            eprintln!(
                "skipping linked reflink restore: {}",
                String::from_utf8_lossy(&probe.stderr)
            );
            return Ok(());
        }
    }
    for checkout in [a, b] {
        write_smoke_package(checkout)?;
    }
    let executable_a = &test_artifacts(&mut build_smoke(&fixture, a))?["smoke"];
    let executable_b = &test_artifacts(&mut build_smoke(&fixture, b))?["smoke"];
    let expected = fs::read(executable_a)?;
    let expected_mode = fs::metadata(executable_a)?.mode() & 0o777;
    let old_source = test.tempdir.path().join("old.c");
    fs::write(
        &old_source,
        "#include <unistd.h>\nint main(void) { sleep(120); return 0; }\n",
    )?;
    let old = b.join("target/debug/old");
    Command::new("cc")
        .arg(&old_source)
        .arg("-o")
        .arg(&old)
        .assert()
        .try_success()?;
    fs::rename(&old, executable_b)?;
    let uplift = b.join("target/debug/smoke");
    fs::hard_link(executable_b, &uplift)?;
    let old_bytes = fs::read(&uplift)?;
    let old_inode = fs::metadata(&uplift)?.ino();
    assert_eq!(old_inode, fs::metadata(executable_b)?.ino());
    assert_ne!(expected, old_bytes);

    fs::write(b.join("src/main.rs"), SMOKE_TEST)?;
    zero_incremental_stats()?;
    let mut running = Command::new(executable_b).spawn()?;
    let build = test_artifacts(&mut build_smoke(&fixture, b));
    let state = running.try_wait();
    let _ = running.kill();
    running.wait()?;
    build?;
    assert!(state?.is_none());
    incremental_stats(test, 1, 0)?;
    assert_ne!(fs::metadata(executable_b)?.ino(), old_inode);
    assert_eq!(fs::metadata(executable_b)?.mode() & 0o777, expected_mode);
    assert_eq!(fs::read(executable_b)?, expected);
    assert_eq!(fs::metadata(&uplift)?.ino(), old_inode);
    assert_eq!(fs::read(&uplift)?, old_bytes);
    run_harness(executable_b, "linked harness")?;
    Ok(())
}

#[test]
#[serial]
fn test_rust_cargo_linked_harness_finalized_session_bypasses() -> Result<()> {
    let Some(linker) = Linker::probe("rust-lld")? else {
        return Ok(());
    };
    let fixture = Fixture::new(
        linker,
        "tests",
        &[
            ("SCCACHE_RUST_INCREMENTAL", "cache-no-bypass"),
            ("SCCACHE_RUST_INCREMENTAL_FIRST_BUILD", "incremental"),
        ],
    )?;
    let Fixture { test, a, .. } = &fixture;
    write_smoke_package(a)?;
    let build = || {
        let mut cmd = build_smoke(&fixture, a);
        cmd.env("CARGO_INCREMENTAL", "1");
        cmd
    };
    test_artifacts(&mut build())?;
    assert_eq!(incremental_stats(test, 0, 1)?["cache_writes"], 1);
    super::incremental_sessions(a, "smoke")?;
    fs::write(
        a.join("src/main.rs"),
        format!("{SMOKE_TEST}\n// force rebuild\n"),
    )?;
    zero_incremental_stats()?;
    test_artifacts(&mut build())?;
    let stats = incremental_stats(test, 0, 0)?;
    assert_eq!(stats["not_cached"]["incremental session"], 1);
    assert_eq!(stats["cache_writes"], 0);
    Ok(())
}

#[test]
#[serial]
fn test_rust_cargo_linked_all_bins_examples_and_build_scripts() -> Result<()> {
    let Some(linker) = Linker::probe("rust-lld")? else {
        return Ok(());
    };
    let fixture = Fixture::new(linker, "all", &[])?;
    let Fixture { test, a, b, .. } = &fixture;
    for checkout in [a, b] {
        write_package(
            checkout,
            "linked-bin",
            "[workspace]",
            &[
                (
                    "src/main.rs",
                    "fn main() { println!(\"{}\", env!(\"BUILD_VALUE\")); }",
                ),
                (
                    "examples/example.rs",
                    "fn main() { println!(\"example\"); }",
                ),
                (
                    "build.rs",
                    r#"
                    fn main() {
                        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
                        std::fs::write(out.join("generated.txt"), std::env::current_dir().unwrap().as_os_str().as_encoded_bytes()).unwrap();
                        println!("cargo:rustc-env=BUILD_VALUE=42");
                    }
                "#,
                ),
            ],
        )?;
    }
    for (checkout, hits, misses) in [(a, 0, 3), (b, 3, 0)] {
        zero_incremental_stats()?;
        fixture
            .cargo(checkout)
            .args(["build", "--offline", "--bins", "--examples"])
            .assert()
            .try_success()?;
        incremental_stats(test, hits, misses)?;
        for (path, output) in [("linked-bin", "42\n"), ("examples/example", "example\n")] {
            Command::new(checkout.join("target/debug").join(path))
                .assert()
                .try_success()?
                .try_stdout(output)?;
        }
        let build_dir = checkout.join("target/debug/build");
        let generated: Vec<_> = fs::read_dir(build_dir)?
            .filter_map(|entry| {
                let path = entry.unwrap().path().join("out/generated.txt");
                path.is_file().then_some(path)
            })
            .collect();
        let [generated] = generated.as_slice() else {
            bail!("expected one generated build-script output: {generated:?}");
        };
        assert_eq!(
            fs::read(generated)?,
            checkout.as_os_str().as_encoded_bytes()
        );
    }
    Ok(())
}

#[test_case(false ; "daemon")]
#[test_case(true ; "client_side")]
#[serial]
fn test_rust_cargo_linked_harness_size_cap(client_side: bool) -> Result<()> {
    let Some(linker) = Linker::probe("rust-lld")? else {
        return Ok(());
    };
    let fixture = Fixture::new(
        linker,
        "tests",
        &[("SCCACHE_RUST_CACHE_LINKED_MAX_ENTRY", "1024")],
    )?;
    let Fixture { test, a, b, .. } = &fixture;
    for checkout in [a, b] {
        write_smoke_package(checkout)?;
        let mut cmd = build_smoke(&fixture, checkout);
        if client_side {
            cmd.env("SCCACHE_CLIENT_SIDE", "1");
        }
        let artifacts = test_artifacts(&mut cmd)?;
        assert!(fs::metadata(&artifacts["smoke"])?.len() > 1024);
        run_harness(&artifacts["smoke"], "linked harness")?;
    }
    let stats = incremental_stats(test, 0, 2)?;
    assert_eq!(stats["rust_linked_entry_too_large"], 2);
    assert_eq!(stats["cache_writes"], 0);
    assert_eq!(stats["cache_write_errors"], 0);
    Ok(())
}

fn native_archive(root: &Path, value: u8) -> Result<Vec<u8>> {
    fs::write(
        root.join("answer.c"),
        format!("int answer(void) {{ return {value}; }}\n"),
    )?;
    Command::new("cc")
        .args(["-c", "answer.c", "-o", "answer.o"])
        .current_dir(root)
        .assert()
        .try_success()?;
    Command::new("ar")
        .args(["crs", "replacement.a", "answer.o"])
        .current_dir(root)
        .assert()
        .try_success()?;
    Ok(fs::read(root.join("replacement.a"))?)
}

#[test]
#[serial]
fn test_rust_cargo_linked_concurrent_digest_memo() -> Result<()> {
    let Some(linker) = Linker::probe("rust-lld")? else {
        return Ok(());
    };
    let driver = linker.driver;
    let fixture = Fixture::new(linker, "tests", &[])?;
    let Fixture { test, a, .. } = &fixture;
    write_package(
        a,
        "concurrent",
        "[workspace]",
        &[(
            "src/main.rs",
            r#"
            unsafe extern "C" { fn answer() -> i32; }
            #[test] fn native() { println!("native value: {}", unsafe { answer() }); }
        "#,
        )],
    )?;
    let root = test.tempdir.path();
    let native_build = root.join("native-build");
    fs::create_dir(&native_build)?;
    let shared = root.join("shared");
    fs::create_dir(&shared)?;
    let archive = shared.join("libanswer.a");
    fs::write(&archive, native_archive(&native_build, 42)?)?;
    let original = fs::metadata(&archive)?;
    let original_mtime = filetime::FileTime::from_last_modification_time(&original);
    let update_archive = |value| -> Result<()> {
        let bytes = native_archive(&native_build, value)?;
        assert_eq!(original.len(), bytes.len() as u64);
        fs::write(&archive, bytes)?;
        filetime::set_file_mtime(&archive, original_mtime)?;
        let updated = fs::metadata(&archive)?;
        assert_eq!(updated.ino(), original.ino());
        assert_eq!(
            filetime::FileTime::from_last_modification_time(&updated),
            original_mtime
        );
        assert_ne!(
            (updated.ctime(), updated.ctime_nsec()),
            (original.ctime(), original.ctime_nsec())
        );
        Ok(())
    };
    let hold = root.join("hold");
    fixture.write_wrapper(&format!(
        r#"for arg do
    case "$arg" in
        -Wl,--dependency-file=*)
            if [ -f "$LINK_TEST_GATE/hold" ]; then
                {driver} "$@" || exit
                touch "$LINK_TEST_GATE/${{CARGO_TARGET_DIR##*/}}"
                n=0
                while [ -f "$LINK_TEST_GATE/hold" ] && [ "$n" -lt 600 ]; do
                    sleep 0.1
                    n=$((n+1))
                done
                test "$n" -lt 600
                exit
            fi ;;
    esac
done"#,
    ))?;
    let build = |target: &str| -> Result<PathBuf> {
        let mut flag = OsString::from("-Clink-arg=");
        flag.push(&archive);
        let artifacts = test_artifacts(
            fixture
                .cargo_with_flags(a, [flag])
                .args(["test", "--offline", "--no-run"])
                .env("LINK_TEST_GATE", root)
                .env("CARGO_TARGET_DIR", a.join(target)),
        )?;
        Ok(artifacts["concurrent"].clone())
    };
    let [first, second] = ["target-first", "target-second"];
    let paths = [build(first)?, build(second)?];
    incremental_stats(test, 0, 2)?;

    // A freshly modified file is ineligible for the memo.
    std::thread::sleep(Duration::from_millis(2100));
    zero_incremental_stats()?;
    for (path, target) in paths.iter().zip([first, second]) {
        fs::remove_file(path)?;
        run_harness(&build(target)?, "native value: 42")?;
    }
    incremental_stats(test, 2, 0)?;

    update_archive(43)?;
    for path in &paths {
        fs::remove_file(path)?;
    }
    zero_incremental_stats()?;
    fs::write(&hold, "")?;
    std::thread::scope(|scope| -> Result<()> {
        let first_build = scope.spawn(|| build(first));
        let second_build = scope.spawn(|| build(second));
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = (|| {
            while ![first, second]
                .iter()
                .all(|target| root.join(target).is_file())
            {
                if first_build.is_finished()
                    || second_build.is_finished()
                    || Instant::now() > deadline
                {
                    bail!(
                        "both Cargo runs must reach the linker after rejecting the memoized digest"
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            update_archive(44)
        })();
        fs::remove_file(&hold)?;
        let first_path = first_build.join().unwrap()?;
        let second_path = second_build.join().unwrap()?;
        result?;
        for path in [&first_path, &second_path] {
            run_harness(path, "native value: 43")?;
        }
        Ok(())
    })?;
    let stats = incremental_stats(test, 0, 0)?;
    assert_eq!(stats["non_cacheable_compilations"], 2);
    assert_eq!(stats["cache_writes"], 0);

    zero_incremental_stats()?;
    for (path, target) in paths.iter().zip([first, second]) {
        fs::remove_file(path)?;
        run_harness(&build(target)?, "native value: 44")?;
    }
    assert_eq!(incremental_stats(test, 0, 2)?["cache_writes"], 2);
    Ok(())
}
