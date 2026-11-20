//! System tests for compiling Rust code with cargo.
//!
//! Any copyright is dedicated to the Public Domain.
//! http://creativecommons.org/publicdomain/zero/1.0/

pub mod helpers;

use anyhow::{Context, Result, bail};
use helpers::{CARGO, CRATE_DIR, cargo_clean, stop_sccache};

use assert_cmd::prelude::*;
use fs_err as fs;
use helpers::{SCCACHE_BIN, SccacheTest};
use predicates::prelude::*;
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::process::Command;
use test_case::test_case;

#[macro_use]
extern crate log;

#[test]
#[serial]
fn test_rust_cargo_check() -> Result<()> {
    test_rust_cargo_cmd("check", SccacheTest::new(None)?)
}

#[test]
#[serial]
fn test_rust_cargo_check_readonly() -> Result<()> {
    test_rust_cargo_cmd_readonly("check", SccacheTest::new(None)?)
}

#[test]
#[serial]
fn test_rust_cargo_build() -> Result<()> {
    test_rust_cargo_cmd("build", SccacheTest::new(None)?)
}

#[test]
#[serial]
fn test_rust_cargo_build_readonly() -> Result<()> {
    test_rust_cargo_cmd_readonly("build", SccacheTest::new(None)?)
}

#[test]
#[serial]
#[cfg(unix)]
fn test_run_log_no_perm() -> Result<()> {
    trace!("sccache with log");
    stop_sccache()?;
    let mut cmd = Command::new(SCCACHE_BIN.as_os_str());
    cmd.arg("gcc")
        .env("SCCACHE_ERROR_LOG", "/no-perm.log") // Should not work
        .env("SCCACHE_LOG", "debug");

    cmd.assert().failure().stderr(predicate::str::contains(
        "Cannot open/write log file '/no-perm.log'",
    ));
    Ok(())
}

#[test]
#[serial]
fn test_run_log() -> Result<()> {
    trace!("sccache with log");
    stop_sccache()?;

    let tempdir = tempfile::Builder::new()
        .prefix("sccache_test_rust_cargo")
        .tempdir()
        .context("Failed to create tempdir")?;
    let tmppath = tempdir.path().join("perm.log");
    let mut cmd = Command::new(SCCACHE_BIN.as_os_str());
    cmd.arg("--start-server")
        .env("SCCACHE_ERROR_LOG", &tmppath) // Should not work
        .env("SCCACHE_LOG", "debug");

    cmd.assert().success();
    stop_sccache()?;
    assert!(Path::new(&tmppath).is_file());
    Ok(())
}

/// This test checks that changing an environment variable reference by env! is detected by
/// sccache, causes a rebuild and is correctly printed to stdout.
#[test]
#[serial]
fn test_rust_cargo_run_with_env_dep_parsing() -> Result<()> {
    test_rust_cargo_env_dep(SccacheTest::new(None)?)
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_check_nightly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd(
        "check",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_check_nightly_readonly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd_readonly(
        "check",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_build_nightly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd(
        "build",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

#[cfg(feature = "unstable")]
#[test]
#[serial]
fn test_rust_cargo_build_nightly_readonly() -> Result<()> {
    use std::ffi::OsString;

    test_rust_cargo_cmd_readonly(
        "build",
        SccacheTest::new(Some(&[(
            "RUSTFLAGS",
            OsString::from("-Cprofile-generate=."),
        )]))?,
    )
}

/// Test that building a simple Rust crate with cargo using sccache results in a cache hit
/// when built a second time and a cache miss, when the environment variable referenced via
/// env! is changed.
fn test_rust_cargo_cmd(cmd: &str, test_info: SccacheTest) -> Result<()> {
    // `cargo clean` first, just to be sure there's no leftover build objects.
    cargo_clean(&test_info)?;

    // Now build the crate with cargo.
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_success()?;
    // Clean it so we can build it again.
    cargo_clean(&test_info)?;
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=always"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_success()?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;

    Ok(())
}

fn restart_sccache(
    test_info: &SccacheTest,
    additional_envs: Option<Vec<(String, String)>>,
) -> Result<()> {
    let cache_dir = test_info.tempdir.path().join("cache");

    stop_sccache()?;

    trace!("sccache --start-server");

    let mut cmd = Command::new(SCCACHE_BIN.as_os_str());
    cmd.arg("--start-server");
    cmd.env("SCCACHE_DIR", &cache_dir);

    if let Some(additional_envs) = additional_envs {
        cmd.envs(additional_envs);
    }

    cmd.assert()
        .try_success()
        .context("Failed to start sccache server")?;

    Ok(())
}

/// Test that building a simple Rust crate with cargo using sccache results in the following behaviors (for three different runs):
/// - In read-only mode, a cache miss.
/// - In read-write mode, a cache miss.
/// - In read-only mode, a cache hit.
///
/// The environment variable for read/write mode is added by this function.
fn test_rust_cargo_cmd_readonly(cmd: &str, test_info: SccacheTest) -> Result<()> {
    // `cargo clean` first, just to be sure there's no leftover build objects.
    cargo_clean(&test_info)?;

    // The cache must be put into read-only mode, and that can only be configured
    // when the server starts up, so we need to restart it.
    restart_sccache(
        &test_info,
        Some(vec![("SCCACHE_LOCAL_RW_MODE".into(), "READ_ONLY".into())]),
    )?;

    // Now build the crate with cargo.
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_success()?;

    // Stats reset on server restart, so this needs to be run for each build.
    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(r#""cache_hits":{"counts":{},"adv_counts":{}}"#).from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(
                r#""cache_misses":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;

    cargo_clean(&test_info)?;
    restart_sccache(
        &test_info,
        Some(vec![("SCCACHE_LOCAL_RW_MODE".into(), "READ_WRITE".into())]),
    )?;
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=always"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_success()?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(r#""cache_hits":{"counts":{},"adv_counts":{}}"#).from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(
                r#""cache_misses":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;

    cargo_clean(&test_info)?;
    restart_sccache(
        &test_info,
        Some(vec![("SCCACHE_LOCAL_RW_MODE".into(), "READ_ONLY".into())]),
    )?;
    Command::new(CARGO.as_os_str())
        .args([cmd, "--color=always"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_success()?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(r#""cache_misses":{"counts":{},"adv_counts":{}}"#)
                .from_utf8(),
        )?
        .try_success()?;

    Ok(())
}

fn test_rust_cargo_env_dep(test_info: SccacheTest) -> Result<()> {
    cargo_clean(&test_info)?;
    // Now build the crate with cargo.
    Command::new(CARGO.as_os_str())
        .args(["run", "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_stdout(predicates::str::contains("Env var: 1"))?
        .try_success()?;
    // Clean it so we can build it again.
    cargo_clean(&test_info)?;

    Command::new(CARGO.as_os_str())
        .args(["run", "--color=always"])
        .envs(test_info.env.iter().cloned())
        .env("TEST_ENV_VAR", "OTHER_VALUE")
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8())?
        .try_stdout(predicates::str::contains("Env var: OTHER_VALUE"))?
        .try_success()?;

    // Now get the stats and ensure that we had one cache hit for the second build.
    // The test crate has one dependency (itoa) so there are two separate compilations, but only
    // itoa should be cached (due to the changed environment variable).
    test_info
        .show_stats()?
        .try_stdout(predicates::str::contains(r#""cache_hits":{"counts":{"Rust":1}"#).from_utf8())?
        .try_success()?;

    drop(test_info);
    Ok(())
}

/// Test that building a simple Rust crate with cargo using sccache in read-only mode with an empty cache results in
/// a cache miss that is produced by the readonly storage wrapper (and does not attempt to write to the underlying cache).
#[test]
#[serial]
fn test_rust_cargo_cmd_readonly_preemtive_block() -> Result<()> {
    let test_info = SccacheTest::new(None)?;
    // `cargo clean` first, just to be sure there's no leftover build objects.
    cargo_clean(&test_info)?;

    let sccache_log = test_info.tempdir.path().join("sccache.log");

    stop_sccache()?;

    restart_sccache(
        &test_info,
        Some(vec![
            ("SCCACHE_LOCAL_RW_MODE".into(), "READ_ONLY".into()),
            ("SCCACHE_LOG".into(), "trace".into()),
            (
                "SCCACHE_ERROR_LOG".into(),
                sccache_log.to_str().unwrap().into(),
            ),
        ]),
    )?;

    // Now build the crate with cargo.
    // Assert that our cache miss is due to the readonly storage wrapper, not due to the underlying disk cache.
    Command::new(CARGO.as_os_str())
        .args(["build", "--color=never"])
        .envs(test_info.env.iter().cloned())
        .current_dir(CRATE_DIR.as_os_str())
        .assert()
        .try_stderr(predicates::str::contains("\x1b[").from_utf8().not())?
        .try_success()?;

    let log_contents = fs::read_to_string(sccache_log)?;
    assert!(
        predicates::str::contains("server has setup with ReadOnly").eval(log_contents.as_str())
    );
    assert!(
        predicates::str::contains("Error executing cache write: Cannot write to read-only storage")
            .eval(log_contents.as_str())
    );
    assert!(
        predicates::str::contains("DiskCache::finish_put")
            .not()
            .eval(log_contents.as_str())
    );

    // Stats reset on server restart, so this needs to be run for each build.
    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(r#""cache_hits":{"counts":{},"adv_counts":{}}"#).from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(
                r#""cache_misses":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;
    Ok(())
}

/// Copy the test crate to `name` under the test tempdir
/// and build it there with its own target directory.
///
/// Returns the checkout's target directory.
fn build_checkout(test_info: &SccacheTest, name: &str) -> Result<PathBuf> {
    let checkout = test_info.tempdir.path().join(name);
    fs::create_dir_all(checkout.join("src"))?;
    for file in ["Cargo.toml", "Cargo.lock", "src/lib.rs", "src/bin.rs"] {
        fs::copy(CRATE_DIR.join(file), checkout.join(file))?;
    }
    cargo_build(test_info, &checkout)
}

/// Build the crate in `checkout` with its own target directory.
///
/// Returns the checkout's target directory.
fn cargo_build(test_info: &SccacheTest, checkout: &Path) -> Result<PathBuf> {
    let target_dir = checkout.join("target");
    Command::new(CARGO.as_os_str())
        // `--target-dir` rather than `CARGO_TARGET_DIR`,
        // since cargo forwards `CARGO_*` to rustc and sccache hashes them.
        .args(["build", "--color=never", "--target-dir"])
        .arg(&target_dir)
        .envs(test_info.env.iter().cloned())
        .current_dir(checkout)
        .assert()
        .try_success()?;
    Ok(target_dir)
}

/// Read the dep-info file cargo keeps for the crate `name` under `target_dir`.
fn read_dep_info(target_dir: &Path, name: &str) -> Result<String> {
    /// Search rather than assume `target/debug/deps`,
    /// since cargo changed the build-dir layout in rust-lang/cargo#15010.
    fn collect(dir: &Path, prefix: &str, found: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                collect(&path, prefix, found)?;
            } else if path.extension().is_some_and(|e| e == "d")
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
            {
                found.push(path);
            }
        }
        Ok(())
    }

    let mut found = Vec::new();
    collect(target_dir, &format!("{name}-"), &mut found)?;
    found.sort();

    // One profile and no `--target`, so the crate is built exactly once.
    match found.as_slice() {
        [dep_info] => Ok(fs::read_to_string(dep_info)?),
        [] => bail!("no {name} dep-info file under {}", target_dir.display()),
        _ => bail!(
            "several {name} dep-info files under {}: {found:?}",
            target_dir.display()
        ),
    }
}

/// The same dependency built from two checkouts, the second one being a cache hit.
/// Whose paths does the second checkout's dep-info name? (mozilla/sccache#984)
#[test]
#[serial]
fn test_rust_cargo_dep_info_from_other_checkout() -> Result<()> {
    let test_info = SccacheTest::new(None)?;

    let target_a = build_checkout(&test_info, "a")?;
    let target_b = build_checkout(&test_info, "b")?;

    // Only `itoa` is shared;
    // the crate's own lib and bin are keyed on their differing cwd.
    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":1},"adv_counts":{"rust":1}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;

    let dep_info = read_dep_info(&target_b, "itoa")?;
    assert!(
        !dep_info.contains(target_a.to_str().unwrap()),
        "dep-info names checkout a:\n{dep_info}"
    );
    assert!(
        dep_info.contains(target_b.to_str().unwrap()),
        "dep-info does not name checkout b:\n{dep_info}"
    );
    Ok(())
}

/// Write a crate to `checkout` whose path dependency includes build script output,
/// so the checkout's path reaches both crates' keys through the cwd,
/// `CARGO_MANIFEST_DIR` and the dependency's `OUT_DIR` env-dep.
fn write_out_dir_crate(checkout: &Path) -> Result<()> {
    let package = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
    };
    fs::create_dir_all(checkout.join("src"))?;
    fs::create_dir_all(checkout.join("dep/src"))?;
    fs::write(
        checkout.join("Cargo.toml"),
        package("out-dir-test") + "\n[dependencies]\ndep = { path = \"dep\" }\n",
    )?;
    fs::write(
        checkout.join("src/lib.rs"),
        "pub fn answer() -> u32 { dep::ANSWER }\n",
    )?;
    fs::write(checkout.join("dep/Cargo.toml"), package("dep"))?;
    fs::write(
        checkout.join("dep/build.rs"),
        r#"fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("answer.rs"), "pub const ANSWER: u32 = 42;").unwrap();
}
"#,
    )?;
    fs::write(
        checkout.join("dep/src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/answer.rs\"));\n",
    )?;
    Ok(())
}

/// The test's tempdir by its real path, which is how cargo names it in `CARGO_MANIFEST_DIR`
/// and `[env]` paths; on macOS the tempdir is under a symlink to /private.
fn real_tempdir(test_info: &SccacheTest) -> Result<PathBuf> {
    let path = test_info.tempdir.path();
    if cfg!(unix) {
        Ok(fs::canonicalize(path)?)
    } else {
        Ok(path.to_owned())
    }
}

/// A crate and its path dependency, built from two checkouts listed in `SCCACHE_BASEDIRS`,
/// hit in the second one, whose dep-info still names its own files.
#[test]
#[serial]
fn test_rust_cargo_basedirs_other_checkout() -> Result<()> {
    let test_info = SccacheTest::new(None)?;
    let tempdir = real_tempdir(&test_info)?;
    let [a, b] = ["a", "b"].map(|name| tempdir.join(name));
    let basedirs = std::env::join_paths([&a, &b])?.into_string().unwrap();
    restart_sccache(
        &test_info,
        Some(vec![("SCCACHE_BASEDIRS".into(), basedirs)]),
    )?;

    write_out_dir_crate(&a)?;
    cargo_build(&test_info, &a)?;
    Command::new(SCCACHE_BIN.as_os_str())
        .arg("--zero-stats")
        .assert()
        .try_success()?;
    write_out_dir_crate(&b)?;
    let target_b = cargo_build(&test_info, &b)?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(r#""cache_misses":{"counts":{},"adv_counts":{}}"#)
                .from_utf8(),
        )?
        .try_success()?;

    let dep_info = read_dep_info(&target_b, "dep")?;
    assert!(
        !dep_info.contains(a.to_str().unwrap()),
        "dep-info names checkout a:\n{dep_info}"
    );
    assert!(
        dep_info.contains(b.to_str().unwrap()),
        "dep-info does not name checkout b:\n{dep_info}"
    );

    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
#[serial]
fn test_rust_cargo_disk_reflink_other_checkout() -> Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let probe_dir = tempfile::tempdir()?;
    let mut source = tempfile::NamedTempFile::new_in(probe_dir.path())?;
    source.write_all(&[0; 4096])?;
    let dest = tempfile::NamedTempFile::new_in(probe_dir.path())?;
    // SAFETY: FICLONE takes a source fd by value and both files remain open.
    if unsafe { libc::ioctl(dest.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == -1 {
        eprintln!(
            "skipping cargo reflink check: FICLONE failed: {}",
            std::io::Error::last_os_error()
        );
        return Ok(());
    }

    let test_info = SccacheTest::new(None)?;
    let root = real_tempdir(&test_info)?;
    let [a, b] = ["a", "b"].map(|name| root.join(name));
    let config = root.join("sccache.toml");
    let log = root.join("server.log");
    fs::write(&config, "[cache.disk]\nreflink = true\n")?;
    restart_sccache(
        &test_info,
        Some(vec![
            ("SCCACHE_CONF".into(), config.to_str().unwrap().into()),
            (
                "SCCACHE_BASEDIRS".into(),
                std::env::join_paths([&a, &b])?.into_string().unwrap(),
            ),
            ("SCCACHE_LOG".into(), "info".into()),
            ("SCCACHE_ERROR_LOG".into(), log.to_str().unwrap().into()),
        ]),
    )?;
    write_out_dir_crate(&a)?;
    let target_a = cargo_build(&test_info, &a)?;
    let mut raw_entries = 0;
    for entry in walkdir::WalkDir::new(root.join("cache")) {
        let entry = entry?;
        if entry.file_type().is_file() && entry.file_name() != "CACHEDIR.TAG" {
            let zip = zip::ZipArchive::new(fs::File::open(entry.path())?)?;
            assert!(
                zip.file_names()
                    .any(|name| name.unwrap().starts_with("raw/"))
            );
            raw_entries += 1;
        }
    }
    assert_eq!(raw_entries, 2);

    Command::new(SCCACHE_BIN.as_os_str())
        .arg("--zero-stats")
        .assert()
        .try_success()?;
    write_out_dir_crate(&b)?;
    let target_b = cargo_build(&test_info, &b)?;

    let mut rlibs = 0;
    for entry in walkdir::WalkDir::new(&target_b) {
        let path = entry?.into_path();
        if path.extension().is_some_and(|ext| ext == "rlib") {
            let relative = path.strip_prefix(&target_b)?;
            assert_eq!(
                fs::read(&path)?,
                fs::read(target_a.join(relative))?,
                "{relative:?}"
            );
            rlibs += 1;
        }
    }
    assert!(rlibs >= 2);
    test_info
        .show_stats()?
        .try_stdout(predicates::str::contains(r#""cache_hits":{"counts":{"Rust":2}"#).from_utf8())?
        .try_stdout(
            predicates::str::contains(r#""cache_misses":{"counts":{},"adv_counts":{}}"#)
                .from_utf8(),
        )?
        .try_success()?;
    let log = fs::read_to_string(log)?;
    assert!(!log.contains("Disk cache reflink failed"), "{log}");
    eprintln!("verified {rlibs} byte-equal rlibs, two Rust hits and no reflink fallback");
    Ok(())
}

/// With `SCCACHE_BASEDIRS_REMAP_PATHS`, the outputs a hit restores in the second checkout
/// do not name the first one.
#[test_case(false ; "configured basedirs")]
#[test_case(true ; "compile basedirs")]
#[serial]
fn test_rust_cargo_basedirs_remap_paths(compile_basedirs: bool) -> Result<()> {
    let mut test_info = SccacheTest::new(None)?;
    test_info
        .env
        .push(("SCCACHE_BASEDIRS_REMAP_PATHS", "1".into()));
    let tempdir = real_tempdir(&test_info)?;
    let [a, b] = ["a", "b"].map(|name| tempdir.join(name));
    if !compile_basedirs {
        let basedirs = std::env::join_paths([&a, &b])?.into_string().unwrap();
        restart_sccache(
            &test_info,
            Some(vec![("SCCACHE_BASEDIRS".into(), basedirs)]),
        )?;
    }
    for checkout in [&a, &b] {
        write_out_dir_crate(checkout)?;
        if compile_basedirs {
            write_compile_basedirs_config(checkout)?;
        }
        cargo_build(&test_info, checkout)?;
    }

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;
    let a = a.to_str().unwrap().as_bytes();
    for entry in walkdir::WalkDir::new(b.join("target")) {
        let path = entry?.into_path();
        if path
            .extension()
            .is_some_and(|e| e == "rlib" || e == "rmeta")
        {
            let bytes = fs::read(&path)?;
            assert!(
                !bytes.windows(a.len()).any(|w| w == a),
                "{} names checkout a",
                path.display()
            );
        }
    }
    Ok(())
}

fn incremental_checkouts(
    mode: &str,
    client_side: bool,
) -> Result<(SccacheTest<'static>, [PathBuf; 4])> {
    let test_info = SccacheTest::new(Some(&[
        ("SCCACHE_BASEDIRS_REMAP_PATHS", "1".into()),
        (
            "SCCACHE_CLIENT_SIDE",
            if client_side { "1" } else { "0" }.into(),
        ),
    ]))?;
    let tempdir = real_tempdir(&test_info)?;
    let checkouts = ["a", "b", "c", "d"].map(|name| tempdir.join(name));
    restart_incremental_server(&test_info, &checkouts, mode)?;

    for checkout in &checkouts {
        fs::create_dir_all(checkout.join("src"))?;
        fs::create_dir_all(checkout.join("la/src"))?;
        fs::copy(CRATE_DIR.join("Cargo.lock"), checkout.join("Cargo.lock"))?;
        let mut manifest = fs::read_to_string(CRATE_DIR.join("Cargo.toml"))?;
        manifest.push_str("\n[dependencies.la]\npath = \"la\"\n");
        fs::write(checkout.join("Cargo.toml"), manifest)?;
        fs::write(
            checkout.join("la/Cargo.toml"),
            "[package]\nname = \"la\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )?;
        fs::write(
            checkout.join("src/bin.rs"),
            "extern crate mylib;\nfn main() { println!(\"{}\", mylib::answer()); }\n",
        )?;
        edit_la(checkout, 10)?;
        edit_mylib(checkout, 0)?;
    }
    Ok((test_info, checkouts))
}

fn restart_incremental_server(test: &SccacheTest, checkouts: &[PathBuf], mode: &str) -> Result<()> {
    let basedirs = std::env::join_paths(checkouts)?.into_string().unwrap();
    restart_sccache(
        test,
        Some(vec![
            ("SCCACHE_BASEDIRS".into(), basedirs),
            ("SCCACHE_RUST_INCREMENTAL".into(), mode.into()),
        ]),
    )
}

fn edit_la(checkout: &Path, value: u32) -> Result<()> {
    fs::write(
        checkout.join("la/src/lib.rs"),
        format!("pub fn answer() -> u32 {{ {value} }}\n"),
    )?;
    Ok(())
}

fn edit_mylib(checkout: &Path, delta: u32) -> Result<()> {
    fs::write(
        checkout.join("src/lib.rs"),
        format!(
            "extern crate la;\nfn incremental_warning() {{}}\n\
             pub fn answer() -> u32 {{ la::answer() + {delta} }}\n"
        ),
    )?;
    Ok(())
}

fn incremental_cargo(test: &SccacheTest, checkout: &Path, cmd: &str) -> Command {
    let mut cargo = Command::new(CARGO.as_os_str());
    cargo
        .args([cmd, "--offline", "--color=never", "--target-dir"])
        .arg(checkout.join("target"))
        .envs(test.env.iter().cloned())
        .env_remove("CARGO_INCREMENTAL")
        // The running daemon is authoritative, including for client-side compiles.
        .env("SCCACHE_RUST_INCREMENTAL", "off")
        .current_dir(checkout);
    cargo
}

fn run_incremental_bin(checkout: &Path, value: u32) -> Result<()> {
    Command::new(
        checkout
            .join("target/debug")
            .join(format!("mybin{}", std::env::consts::EXE_SUFFIX)),
    )
    .assert()
    .try_success()?
    .try_stdout(format!("{value}\n"))?;
    Ok(())
}

fn incremental_crate_dirs(checkout: &Path, name: &str) -> Result<Vec<PathBuf>> {
    let root = checkout.join("target/debug/incremental");
    let mut dirs = Vec::new();
    if root.exists() {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}-"))
            {
                dirs.push(entry.path());
            }
        }
    }
    Ok(dirs)
}

fn private_incremental_dirs(checkout: &Path, name: &str) -> Result<Vec<PathBuf>> {
    let prefix = format!("{name}-sccache-");
    Ok(incremental_crate_dirs(checkout, name)?
        .into_iter()
        .filter(|dir| {
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&prefix)
        })
        .collect())
}

fn assert_no_working_sessions(checkout: &Path, crates: &[&str]) -> Result<()> {
    for name in crates {
        for folder in incremental_crate_dirs(checkout, name)? {
            for entry in walkdir::WalkDir::new(folder).max_depth(2) {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy();
                assert!(
                    !(entry.file_type().is_dir()
                        && name.starts_with("s-")
                        && name.ends_with("-working")),
                    "unexpected preflight session at {:?}",
                    entry.path()
                );
            }
        }
    }
    Ok(())
}

fn assert_cache_only_sessions(checkout: &Path, crates: &[&str]) -> Result<()> {
    assert_no_working_sessions(checkout, crates)?;
    for name in crates {
        assert!(
            private_incremental_dirs(checkout, name)?.is_empty(),
            "unexpected private incremental directory for {name} in {checkout:?}"
        );
    }
    Ok(())
}

fn incremental_sessions(checkout: &Path, name: &str) -> Result<Vec<PathBuf>> {
    assert_no_working_sessions(checkout, &[name])?;
    let dirs = private_incremental_dirs(checkout, name)?;
    assert_eq!(dirs.len(), 1, "crate {name} in {checkout:?}");
    let mut sessions = Vec::new();
    for entry in walkdir::WalkDir::new(&dirs[0]).min_depth(2).max_depth(2) {
        let entry = entry?;
        if entry.file_type().is_dir() && entry.file_name().to_string_lossy().starts_with("s-") {
            sessions.push(entry.into_path());
        }
    }
    sessions.sort();
    assert!(!sessions.is_empty(), "crate {name} in {checkout:?}");
    Ok(sessions)
}

#[cfg(unix)]
fn incremental_rmeta(checkout: &Path) -> Result<PathBuf> {
    for entry in walkdir::WalkDir::new(checkout.join("target")) {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy();
        if entry.file_type().is_file() && name.starts_with("libmylib-") && name.ends_with(".rmeta")
        {
            return Ok(entry.into_path());
        }
    }
    bail!("missing mylib .rmeta in {checkout:?}")
}

fn zero_incremental_stats() -> Result<()> {
    Command::new(SCCACHE_BIN.as_os_str())
        .arg("--zero-stats")
        .assert()
        .try_success()?;
    Ok(())
}

fn incremental_stats(test: &SccacheTest, hits: u64, misses: u64) -> Result<serde_json::Value> {
    let output = test.show_stats()?;
    let info: serde_json::Value = serde_json::from_slice(&output.get_output().stdout)?;
    let stats = &info["stats"];
    assert_eq!(
        stats["cache_hits"]["counts"]["Rust"].as_u64().unwrap_or(0),
        hits,
        "{stats}"
    );
    assert_eq!(
        stats["cache_misses"]["counts"]["Rust"]
            .as_u64()
            .unwrap_or(0),
        misses,
        "{stats}"
    );
    Ok(stats.clone())
}

#[test]
#[serial]
fn test_rust_cargo_incremental_off() -> Result<()> {
    let (test, [a, b, ..]) = incremental_checkouts("off", false)?;
    incremental_cargo(&test, &a, "build")
        .env("CARGO_INCREMENTAL", "1")
        .assert()
        .try_success()?;
    zero_incremental_stats()?;
    incremental_cargo(&test, &b, "build")
        .env("CARGO_INCREMENTAL", "1")
        .assert()
        .try_success()?;
    let stats = incremental_stats(&test, 1, 0)?;
    assert_eq!(stats["not_cached"]["incremental"], 2);
    assert_eq!(stats["rust_incremental_not_stored"], 0);
    for name in ["la", "mylib"] {
        assert!(private_incremental_dirs(&b, name)?.is_empty());
        assert_eq!(incremental_crate_dirs(&b, name)?.len(), 1);
    }
    assert_cache_only_sessions(&b, &["itoa"])?;
    run_incremental_bin(&b, 10)
}

#[test]
#[serial]
fn test_rust_cargo_incremental_build_ignores_flat_session_then_bypasses() -> Result<()> {
    let (test, checkouts) = incremental_checkouts("off", false)?;
    let a = &checkouts[0];
    incremental_cargo(&test, a, "build")
        .assert()
        .try_success()?;
    assert!(private_incremental_dirs(a, "mylib")?.is_empty());
    let flat = incremental_crate_dirs(a, "mylib")?;
    assert_eq!(flat.len(), 1);
    assert!(fs::read_dir(&flat[0])?.any(|entry| {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        entry.file_type().unwrap().is_dir() && name.starts_with("s-") && !name.ends_with("-working")
    }));

    restart_incremental_server(&test, &checkouts, "cache")?;
    edit_mylib(a, 1)?;
    incremental_cargo(&test, a, "build")
        .assert()
        .try_success()?;
    let stats = incremental_stats(&test, 0, 1)?;
    assert_eq!(stats["cache_writes"], 1);
    assert_eq!(stats["rust_incremental_not_stored"], 0);
    let before = incremental_sessions(a, "mylib")?;
    let private = &private_incremental_dirs(a, "mylib")?[0];
    let dirs_before_bypass = incremental_crate_dirs(a, "mylib")?;

    let old_mtime = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_mtime(private, old_mtime)?;
    zero_incremental_stats()?;
    edit_mylib(a, 2)?;
    incremental_cargo(&test, a, "build")
        .assert()
        .try_success()?;
    let stats = incremental_stats(&test, 0, 0)?;
    assert_eq!(stats["not_cached"]["incremental session"], 1);
    assert_eq!(stats["cache_writes"], 0);
    assert_ne!(incremental_sessions(a, "mylib")?, before);
    let mut dirs_after_bypass = incremental_crate_dirs(a, "mylib")?;
    dirs_after_bypass.sort();
    let mut dirs_before_bypass = dirs_before_bypass;
    dirs_before_bypass.sort();
    assert_eq!(dirs_after_bypass, dirs_before_bypass);
    assert!(filetime::FileTime::from_last_modification_time(&fs::metadata(private)?) > old_mtime);
    run_incremental_bin(a, 12)?;

    incremental_cargo(&test, a, "clean")
        .args(["-p", "test-crate"])
        .assert()
        .try_success()?;
    assert!(incremental_crate_dirs(a, "mylib")?.is_empty());
    Ok(())
}

#[test_case(false ; "daemon")]
#[test_case(true ; "client side")]
#[serial]
fn test_rust_cargo_incremental_client_bypass_sends_no_request(client_side: bool) -> Result<()> {
    let (test, [a, ..]) = incremental_checkouts("cache", client_side)?;
    let build = || {
        let mut cargo = incremental_cargo(&test, &a, "build");
        cargo
            .env("SCCACHE_RUST_INCREMENTAL", "cache")
            .env("SCCACHE_BASEDIRS", &a);
        cargo
    };
    build().assert().try_success()?;
    assert_eq!(incremental_stats(&test, 0, 3)?["cache_writes"], 3);
    let old = incremental_sessions(&a, "mylib")?;
    let private = private_incremental_dirs(&a, "mylib")?;
    run_incremental_bin(&a, 10)?;

    zero_incremental_stats()?;
    edit_mylib(&a, 1)?;
    build().assert().try_success()?;
    let stats = incremental_stats(&test, 0, 0)?;
    assert_eq!(
        stats["compile_requests"],
        u64::from(!client_side),
        "{stats}"
    );
    assert_eq!(stats["requests_executed"], 0);
    assert_eq!(stats["cache_writes"], 0);
    assert!(stats["not_cached"]["incremental session"].is_null());
    assert_eq!(private_incremental_dirs(&a, "mylib")?, private);
    assert_ne!(incremental_sessions(&a, "mylib")?, old);
    run_incremental_bin(&a, 11)
}

#[test_case(false ; "daemon")]
#[test_case(true ; "client side")]
#[serial]
fn test_rust_cargo_incremental_new_checkout_hits_and_replays_warnings(
    client_side: bool,
) -> Result<()> {
    let (test, [a, b, ..]) = incremental_checkouts("cache", client_side)?;
    for checkout in [&a, &b] {
        incremental_cargo(&test, checkout, "build")
            .assert()
            .try_success()?
            .try_stderr(
                predicates::str::contains("function `incremental_warning` is never used")
                    .from_utf8(),
            )?;
        run_incremental_bin(checkout, 10)?;
    }
    let stats = incremental_stats(&test, 3, 3)?;
    assert_eq!(stats["cache_writes"], 3);
    assert_eq!(stats["rust_incremental_not_stored"], 0);
    assert!(stats["not_cached"]["incremental"].is_null());
    for name in ["la", "mylib"] {
        assert!(incremental_crate_dirs(&b, name)?.is_empty());
    }
    assert_cache_only_sessions(&b, &["itoa", "la", "mylib"])
}

#[test_case("test", "--no-run" ; "test harness")]
#[test_case("check", "--all-targets" ; "check all targets")]
#[serial]
fn test_rust_cargo_incremental_lib_hits_with_test_harness(cmd: &str, flag: &str) -> Result<()> {
    let (test, [a, b, ..]) = incremental_checkouts("cache", false)?;
    incremental_cargo(&test, &a, cmd)
        .args([flag, "-j", "4"])
        .assert()
        .try_success()?;
    let stats = incremental_stats(&test, 0, 3)?;
    assert_eq!(stats["cache_writes"], 3);
    assert_eq!(stats["rust_incremental_not_stored"], 0);
    assert_eq!(private_incremental_dirs(&a, "mylib")?.len(), 1);
    assert_eq!(incremental_crate_dirs(&a, "mylib")?.len(), 2);

    zero_incremental_stats()?;
    incremental_cargo(&test, &b, cmd)
        .args([flag, "-j", "4"])
        .assert()
        .try_success()?;
    incremental_stats(&test, 3, 0)?;
    assert_eq!(incremental_crate_dirs(&b, "mylib")?.len(), 1);
    assert_cache_only_sessions(&b, &["itoa", "la", "mylib"])
}

#[test]
#[serial]
fn test_rust_cargo_incremental_edits_store_only_when_cold() -> Result<()> {
    let (test, [a, b, c, d]) = incremental_checkouts("cache-no-bypass", false)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    incremental_stats(&test, 3, 3)?;
    assert_cache_only_sessions(&b, &["itoa", "la", "mylib"])?;
    assert!(incremental_crate_dirs(&b, "mylib")?.is_empty());

    zero_incremental_stats()?;
    edit_mylib(&b, 1)?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    run_incremental_bin(&b, 11)?;
    let first = incremental_sessions(&b, "mylib")?;
    assert_eq!(first.len(), 1);
    assert_eq!(incremental_stats(&test, 0, 1)?["cache_writes"], 1);

    zero_incremental_stats()?;
    edit_mylib(&b, 2)?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    run_incremental_bin(&b, 12)?;
    let second = incremental_sessions(&b, "mylib")?;
    assert_eq!(first[0].parent(), second[0].parent());
    assert!(second.iter().any(|path| !first.contains(path)));
    let stats = incremental_stats(&test, 0, 1)?;
    assert_eq!(stats["rust_incremental_not_stored"], 1);
    assert_eq!(stats["cache_writes"], 0);
    test.show_text_stats(false)?.try_stdout(
        predicates::str::contains("Incremental: not stored, session reused").from_utf8(),
    )?;

    zero_incremental_stats()?;
    edit_mylib(&c, 2)?;
    incremental_cargo(&test, &c, "build")
        .assert()
        .try_success()?;
    assert_eq!(incremental_stats(&test, 2, 1)?["cache_writes"], 1);
    assert_cache_only_sessions(&c, &["itoa", "la"])?;

    zero_incremental_stats()?;
    edit_mylib(&d, 2)?;
    incremental_cargo(&test, &d, "build")
        .assert()
        .try_success()?;
    incremental_stats(&test, 3, 0)?;
    run_incremental_bin(&d, 12)?;
    assert_cache_only_sessions(&d, &["itoa", "la", "mylib"])
}

#[cfg(unix)]
#[test]
#[serial]
fn test_rust_cargo_incremental_hit_preserves_session_metadata_inode() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let (test, [a, b, c, ..]) = incremental_checkouts("cache-no-bypass", false)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    incremental_stats(&test, 3, 3)?;
    assert_cache_only_sessions(&b, &["itoa", "la", "mylib"])?;

    edit_mylib(&a, 1)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    run_incremental_bin(&a, 11)?;
    let output = incremental_rmeta(&a)?;
    let output_meta = fs::metadata(&output)?;
    let metadata = incremental_sessions(&a, "mylib")?
        .into_iter()
        .map(|session| session.join("metadata.rmeta"))
        .find(|file| fs::metadata(file).is_ok_and(|meta| meta.ino() == output_meta.ino()))
        .context("rustc did not hard-link its output into the incremental session")?;
    let session_bytes = fs::read(&metadata)?;
    let original_links = fs::metadata(&metadata)?.nlink();
    assert!(original_links > 1);

    zero_incremental_stats()?;
    edit_mylib(&a, 0)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    incremental_stats(&test, 1, 0)?;
    run_incremental_bin(&a, 10)?;
    assert_no_working_sessions(&a, &["mylib"])?;
    assert_eq!(fs::metadata(&metadata)?.ino(), output_meta.ino());
    assert_ne!(fs::metadata(&output)?.ino(), output_meta.ino());
    assert_eq!(fs::metadata(&metadata)?.nlink(), original_links - 1);
    assert_eq!(fs::read(&metadata)?, session_bytes);
    assert_ne!(fs::read(&output)?, session_bytes);

    edit_mylib(&a, 1)?;
    incremental_cargo(&test, &a, "build")
        .env("SCCACHE_RECACHE", "1")
        .assert()
        .try_success()?;
    run_incremental_bin(&a, 11)?;

    edit_mylib(&c, 1)?;
    incremental_cargo(&test, &c, "build")
        .env("SCCACHE_NO_CACHE", "1")
        .assert()
        .try_success()?;
    assert_eq!(fs::read(&output)?, fs::read(incremental_rmeta(&c)?)?);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn test_rust_cargo_incremental_edit_against_cache_served_dependency() -> Result<()> {
    let (test, [a, b, c, ..]) = incremental_checkouts("cache-no-bypass", false)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    incremental_stats(&test, 3, 3)?;
    assert_cache_only_sessions(&b, &["itoa", "la", "mylib"])?;

    edit_mylib(&b, 1)?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    run_incremental_bin(&b, 11)?;
    let before = incremental_sessions(&b, "mylib")?;

    edit_la(&a, 20)?;
    incremental_cargo(&test, &a, "clean")
        .args(["-p", "la"])
        .assert()
        .try_success()?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;

    zero_incremental_stats()?;
    edit_la(&b, 20)?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    assert_eq!(
        incremental_stats(&test, 1, 1)?["rust_incremental_not_stored"],
        1
    );
    run_incremental_bin(&b, 21)?;
    assert_cache_only_sessions(&b, &["la"])?;
    assert!(incremental_crate_dirs(&b, "la")?.is_empty());
    assert_ne!(incremental_sessions(&b, "mylib")?, before);

    edit_la(&c, 20)?;
    edit_mylib(&c, 1)?;
    incremental_cargo(&test, &c, "build")
        .env("SCCACHE_NO_CACHE", "1")
        .assert()
        .try_success()?;
    assert_eq!(
        fs::read(incremental_rmeta(&b)?)?,
        fs::read(incremental_rmeta(&c)?)?
    );
    Ok(())
}

#[test]
#[serial]
fn test_rust_cargo_incremental_stale_sessions_survive_hits() -> Result<()> {
    let (test, [a, b, ..]) = incremental_checkouts("cache-no-bypass", false)?;
    edit_la(&a, 20)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    run_incremental_bin(&b, 10)?;
    assert_cache_only_sessions(&b, &["itoa"])?;
    let old_la = incremental_sessions(&b, "la")?;
    let old_mylib = incremental_sessions(&b, "mylib")?;

    zero_incremental_stats()?;
    edit_la(&b, 20)?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    incremental_stats(&test, 2, 0)?;
    run_incremental_bin(&b, 20)?;
    assert_eq!(incremental_sessions(&b, "la")?, old_la);
    assert_eq!(incremental_sessions(&b, "mylib")?, old_mylib);

    zero_incremental_stats()?;
    edit_la(&b, 30)?;
    incremental_cargo(&test, &b, "build")
        .assert()
        .try_success()?;
    assert_eq!(
        incremental_stats(&test, 0, 2)?["rust_incremental_not_stored"],
        2
    );
    assert_ne!(incremental_sessions(&b, "la")?, old_la);
    run_incremental_bin(&b, 30)
}

#[test]
#[serial]
fn test_rust_cargo_incremental_modes_do_not_mix() -> Result<()> {
    let (test, [a, b, c, ..]) = incremental_checkouts("cache", false)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;

    zero_incremental_stats()?;
    incremental_cargo(&test, &b, "build")
        .args(["--config", "profile.dev.incremental=false"])
        .assert()
        .try_success()?;
    incremental_stats(&test, 1, 2)?;
    assert_cache_only_sessions(&b, &["itoa"])?;

    // Cargo also forwards this variable to non-incremental dependencies.
    zero_incremental_stats()?;
    incremental_cargo(&test, &c, "build")
        .env("CARGO_INCREMENTAL", "0")
        .assert()
        .try_success()?;
    incremental_stats(&test, 0, 3)?;

    zero_incremental_stats()?;
    incremental_cargo(&test, &a, "clean")
        .assert()
        .try_success()?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    incremental_stats(&test, 3, 0)?;
    assert_cache_only_sessions(&a, &["itoa", "la", "mylib"])?;
    run_incremental_bin(&a, 10)
}

#[test]
#[serial]
fn test_rust_cargo_incremental_check_hits_then_bypasses_known_session() -> Result<()> {
    let (test, [a, b, ..]) = incremental_checkouts("cache", false)?;
    incremental_cargo(&test, &a, "check")
        .assert()
        .try_success()?;
    incremental_cargo(&test, &b, "check")
        .assert()
        .try_success()?;
    assert_eq!(incremental_stats(&test, 3, 3)?["cache_writes"], 3);
    assert_cache_only_sessions(&b, &["itoa", "la", "mylib"])?;
    assert!(incremental_crate_dirs(&b, "mylib")?.is_empty());

    zero_incremental_stats()?;
    edit_mylib(&b, 1)?;
    incremental_cargo(&test, &b, "check")
        .assert()
        .try_success()?;
    assert_eq!(incremental_stats(&test, 0, 1)?["cache_writes"], 1);
    let old = incremental_sessions(&b, "mylib")?;

    zero_incremental_stats()?;
    edit_mylib(&b, 2)?;
    incremental_cargo(&test, &b, "check")
        .assert()
        .try_success()?;
    let stats = incremental_stats(&test, 0, 0)?;
    assert_eq!(stats["not_cached"]["incremental session"], 1);
    assert_eq!(stats["cache_writes"], 0);
    assert_ne!(incremental_sessions(&b, "mylib")?, old);
    assert_eq!(
        incremental_crate_dirs(&b, "mylib")?,
        private_incremental_dirs(&b, "mylib")?
    );
    test.show_text_stats(false)?
        .try_stdout(predicates::str::contains("incremental session").from_utf8())?;

    incremental_cargo(&test, &b, "clean")
        .args(["-p", "test-crate"])
        .assert()
        .try_success()?;
    let root = b.join("target/debug/incremental");
    assert!(!fs::read_dir(root)?.any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("mylib-")
    }));
    Ok(())
}

/// Test that a crate using a proc-macro hits the cache when the proc-macro was built into a
/// different target directory. On macOS the linker writes the proc-macro dylib's own path into
/// it, so hashing the whole file gave every target directory its own key.
#[test]
#[serial]
fn test_rust_cargo_proc_macro_in_another_target_dir() -> Result<()> {
    let test_info = SccacheTest::new(None)?;
    let root = test_info.tempdir.path();
    let src = root.join("src");
    fs::create_dir_all(src.join("pm/src"))?;
    fs::create_dir_all(src.join("user/src"))?;
    fs::write(
        src.join("Cargo.toml"),
        r#"[workspace]
        members = ["pm", "user"]
        resolver = "2"
        "#,
    )?;
    fs::write(
        src.join("pm/Cargo.toml"),
        r#"[package]
        name = "pm"
        version = "0.1.0"
        edition = "2021"
        [lib]
        proc-macro = true
        "#,
    )?;
    fs::write(
        src.join("pm/src/lib.rs"),
        r#"use proc_macro::TokenStream;
        #[proc_macro]
        pub fn answer(_: TokenStream) -> TokenStream {
            "42".parse().unwrap()
        }
        "#,
    )?;
    fs::write(
        src.join("user/Cargo.toml"),
        r#"[package]
        name = "user"
        version = "0.1.0"
        edition = "2021"
        [dependencies]
        pm = { path = "../pm" }
        "#,
    )?;
    fs::write(
        src.join("user/src/lib.rs"),
        "pub fn answer() -> u32 { pm::answer!() }\n",
    )?;

    // The target directory goes on the command line, because sccache hashes every `CARGO_*`
    // variable and an exported CARGO_TARGET_DIR would itself make the keys differ.
    let env = test_info
        .env
        .iter()
        .filter(|(var, _)| *var != "CARGO_TARGET_DIR")
        .cloned()
        .collect::<Vec<_>>();
    for target_dir in ["t1", "target-two"] {
        Command::new(CARGO.as_os_str())
            .args(["build", "--color=never", "--target-dir"])
            .arg(root.join(target_dir))
            .envs(env.iter().cloned())
            .current_dir(&src)
            .assert()
            .try_success()?;
    }

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":1},"adv_counts":{"rust":1}}"#,
            )
            .from_utf8(),
        )?
        .try_success()?;
    Ok(())
}

#[test]
#[serial]
fn test_rust_cargo_basedirs_per_build() -> Result<()> {
    test_rust_cargo_basedirs_per_build_mode(&[])
}

#[test]
#[serial]
fn test_rust_cargo_basedirs_per_build_client_side() -> Result<()> {
    test_rust_cargo_basedirs_per_build_mode(&[("SCCACHE_CLIENT_SIDE", "1".into())])
}

/// Two checkouts that each name their own root in `SCCACHE_BASEDIRS`
/// through cargo's `[env]`, with a server started without basedirs:
/// the second checkout hits.
fn test_rust_cargo_basedirs_per_build_mode(
    envs: &[(&'static str, std::ffi::OsString)],
) -> Result<()> {
    let test_info = SccacheTest::new(Some(envs))?;
    let tempdir = real_tempdir(&test_info)?;
    let [a, b] = ["a", "b"].map(|name| tempdir.join(name));
    for checkout in [&a, &b] {
        write_out_dir_crate(checkout)?;
        write_compile_basedirs_config(checkout)?;
    }

    cargo_build(&test_info, &a)?;
    Command::new(SCCACHE_BIN.as_os_str())
        .arg("--zero-stats")
        .assert()
        .try_success()?;
    cargo_build(&test_info, &b)?;

    test_info
        .show_stats()?
        .try_stdout(
            predicates::str::contains(
                r#""cache_hits":{"counts":{"Rust":2},"adv_counts":{"rust":2}}"#,
            )
            .from_utf8(),
        )?
        .try_stdout(
            predicates::str::contains(r#""cache_misses":{"counts":{},"adv_counts":{}}"#)
                .from_utf8(),
        )?
        .try_success()?;
    Ok(())
}

fn write_compile_basedirs_config(checkout: &Path) -> Result<()> {
    fs::create_dir_all(checkout.join(".cargo"))?;
    fs::write(
        checkout.join(".cargo/config.toml"),
        "[env]\nSCCACHE_BASEDIRS = { value = \"\", relative = true }\n",
    )?;
    Ok(())
}
