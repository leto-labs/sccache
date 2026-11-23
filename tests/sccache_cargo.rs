//! System tests for compiling Rust code with cargo.
//!
//! Any copyright is dedicated to the Public Domain.
//! http://creativecommons.org/publicdomain/zero/1.0/

pub mod helpers;

#[cfg(target_os = "linux")]
#[path = "sccache_cargo/linked.rs"]
mod linked;

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
        // Keep the environment fixed across checkouts, including in `all` mode.
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

#[test_case("semantic", false; "semantic server")]
#[test_case("semantic", true; "semantic client")]
#[test_case("all", false; "all server")]
#[test_case("all", true; "all client")]
#[serial]
fn test_rust_cargo_hash_cargo_env_other_checkout(mode: &str, client_side: bool) -> Result<()> {
    let test = SccacheTest::new(Some(&[(
        "SCCACHE_CLIENT_SIDE",
        if client_side { "1" } else { "0" }.into(),
    )]))?;
    let root = real_tempdir(&test)?;
    let [a, b] = ["a", "b"].map(|name| root.join(name));
    for checkout in [&a, &b] {
        write_out_dir_crate(checkout)?;
        write_compile_basedirs_config(checkout)?;
        let manifest = checkout.join("Cargo.toml");
        let package = fs::read_to_string(&manifest)?;
        fs::write(manifest, package + "\n[workspace]\nmembers = [\"dep\"]\n")?;
    }
    restart_sccache(
        &test,
        Some(vec![("SCCACHE_RUST_HASH_CARGO_ENV".into(), mode.into())]),
    )?;

    let build = |checkout: &Path, extra_env: &[(&str, &str)]| -> Result<()> {
        Command::new(CARGO.as_os_str())
            .args(["build", "--workspace", "--lib"])
            .envs(test.env.iter().cloned())
            .env("CARGO_TARGET_DIR", checkout.join("target"))
            .env_remove("CARGO_TERM_COLOR")
            .env_remove("CARGO_NET_OFFLINE")
            .envs(extra_env.iter().copied())
            .current_dir(checkout)
            .assert()
            .try_success()?;
        Ok(())
    };
    build(&a, &[])?;
    incremental_stats(&test, 0, 2)?;
    zero_incremental_stats()?;
    build(
        &b,
        &[
            ("CARGO_TERM_COLOR", "always"),
            ("CARGO_NET_OFFLINE", "true"),
        ],
    )?;
    if mode == "semantic" {
        incremental_stats(&test, 2, 0)?;
    } else {
        incremental_stats(&test, 0, 2)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test_case(false; "server compilation")]
#[test_case(true; "client compilation")]
#[serial]
fn test_rust_cargo_disk_reflink_other_checkout(client_side: bool) -> Result<()> {
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

    let test_info = SccacheTest::new(Some(&[(
        "SCCACHE_CLIENT_SIDE",
        if client_side { "1" } else { "0" }.into(),
    )]))?;
    let root = real_tempdir(&test_info)?;
    let cache_dir = root.join("cache");
    let weighted = cache_dir_uses_btrfs_zstd(&cache_dir)?;
    let [a, b] = ["a", "b"].map(|name| root.join(name));
    if weighted && !client_side {
        // The store-side clone keeps the compiler output's extent compression.
        let target = a.join("target");
        fs::create_dir_all(&target)?;
        assert!(cache_dir_uses_btrfs_zstd(&target)?);
    }
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
            ("SCCACHE_LOG".into(), "debug".into()),
            ("SCCACHE_ERROR_LOG".into(), log.to_str().unwrap().into()),
        ]),
    )?;
    let payload_size = if client_side { 9 * 1024 * 1024 } else { 262144 };
    let write_crate = |checkout: &Path| -> Result<()> {
        write_out_dir_crate(checkout)?;
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(checkout.join("src/lib.rs"))?,
            "pub static PAYLOAD: [u8; {payload_size}] = [42; {payload_size}];"
        )?;
        Ok(())
    };
    write_crate(&a)?;
    let target_a = cargo_build(&test_info, &a)?;
    Command::new(SCCACHE_BIN.as_os_str())
        .arg("--zero-stats")
        .assert()
        .try_success()?;
    let mut raw_entries = 0;
    let mut weighted_entries = 0;
    let mut stored_rlib = None;
    for entry in walkdir::WalkDir::new(cache_dir) {
        let entry = entry?;
        if entry.file_type().is_file() && entry.file_name() != "CACHEDIR.TAG" {
            let file = fs::File::open(entry.path())?;
            let mut zip = zip::ZipArchive::new(&file)?;
            assert!(
                zip.file_names()
                    .any(|name| name.unwrap().starts_with("raw/"))
            );
            let mut has_weight = false;
            if weighted {
                let mut weight = [0; 8];
                // SAFETY: The file is open and weight is writable for eight bytes.
                let len = unsafe {
                    libc::fgetxattr(
                        file.as_raw_fd(),
                        c"user.sccache.weight".as_ptr(),
                        weight.as_mut_ptr().cast(),
                        weight.len(),
                    )
                };
                if len == 8 {
                    let weight = u64::from_le_bytes(weight);
                    assert!(weight > 0 && weight < file.metadata()?.len());
                    weighted_entries += 1;
                    has_weight = true;
                }
            }
            for index in 0..zip.len() {
                let member = zip.by_index(index)?;
                if let Some(name) = member.name()?.strip_prefix("raw/")
                    && name.starts_with("libout_dir_test-")
                    && name.ends_with(".rlib")
                {
                    assert!(member.size() >= payload_size);
                    assert!(
                        !weighted || has_weight,
                        "expected a weight for the large rlib"
                    );
                    assert!(stored_rlib.is_none());
                    let offset = member
                        .data_start()
                        .context("cached rlib has no data offset")?;
                    stored_rlib = Some((name.to_owned(), entry.into_path(), offset));
                    break;
                }
            }
            raw_entries += 1;
        }
    }
    assert_eq!(raw_entries, 2);
    let (rlib, entry, offset) = stored_rlib.context("cached rlib missing")?;
    let source = walkdir::WalkDir::new(&target_a)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|entry| entry.file_name() == rlib.as_str())
        .context("rlib missing in checkout A")?;
    let source = source.path().strip_prefix(&target_a)?;
    if !client_side {
        assert_eq!(
            shared_extent_at(&fs::File::open(target_a.join(source))?, 0)?,
            shared_extent_at(&fs::File::open(&entry)?, offset)?,
            "checkout A and the cache must share a physical extent after the miss"
        );
    }

    write_crate(&b)?;
    let target_b = cargo_build(&test_info, &b)?;
    assert_eq!(
        shared_extent_at(&fs::File::open(target_b.join(source))?, 0)?,
        shared_extent_at(&fs::File::open(&entry)?, offset)?,
        "checkout B and the cache must share a physical extent after the hit"
    );

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
    assert_eq!(log.contains("handle_client: storage_put_raw"), client_side);
    let shared = if client_side {
        "B and cache"
    } else {
        "A, B and cache"
    };
    eprintln!(
        "verified {raw_entries} raw entries ({weighted_entries} weighted), \
         {rlibs} byte-equal rlibs, two Rust hits, and a shared FIEMAP extent in {shared}"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn shared_extent_at(file: &fs::File, offset: u64) -> Result<u64> {
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }

    #[repr(C)]
    struct Request {
        header: Fiemap,
        extent: Extent,
    }

    let mut request = Request {
        header: Fiemap {
            start: offset,
            length: 4096,
            flags: 1, // FIEMAP_FLAG_SYNC
            mapped_extents: 0,
            extent_count: 1,
            reserved: 0,
        },
        extent: Extent::default(),
    };
    // SAFETY: The request follows the Linux FIEMAP layout with space for one extent.
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            libc::_IOWR::<Fiemap>(u32::from(b'f'), 11),
            &raw mut request,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error()).context("FIEMAP");
    }
    assert_eq!(request.header.mapped_extents, 1);
    let extent = request.extent;
    assert!((extent.logical..extent.logical + extent.length).contains(&offset));
    assert_ne!(extent.flags & 0x2000, 0, "FIEMAP_EXTENT_SHARED");
    // Encoded ranges refer to the compressed extent as a whole.
    const FIEMAP_EXTENT_ENCODED: u32 = 8;
    if extent.flags & FIEMAP_EXTENT_ENCODED != 0 {
        Ok(extent.physical)
    } else {
        Ok(extent.physical + (offset - extent.logical))
    }
}

#[cfg(target_os = "linux")]
fn cache_dir_uses_btrfs_zstd(path: &Path) -> Result<bool> {
    use std::os::fd::AsRawFd;

    let dir = fs::File::open(path)?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: The open fd and out pointer are valid. Success initializes stat.
    let stat = unsafe {
        if libc::fstatfs(dir.as_raw_fd(), stat.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        stat.assume_init()
    };
    if stat.f_type != libc::BTRFS_SUPER_MAGIC || stat.f_bsize != 4096 {
        return Ok(false);
    }
    // SAFETY: The directory is open and the string literals have static storage.
    if unsafe {
        libc::fsetxattr(
            dir.as_raw_fd(),
            c"btrfs.compression".as_ptr(),
            b"zstd".as_ptr().cast(),
            4,
            0,
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(true)
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
    restart_incremental_server(&test_info, &checkouts, mode, None)?;

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

fn restart_incremental_server(
    test: &SccacheTest,
    checkouts: &[PathBuf],
    mode: &str,
    first_build: Option<&str>,
) -> Result<()> {
    let basedirs = std::env::join_paths(checkouts)?.into_string().unwrap();
    let mut env = vec![
        ("SCCACHE_BASEDIRS".into(), basedirs),
        ("SCCACHE_RUST_INCREMENTAL".into(), mode.into()),
    ];
    if let Some(first_build) = first_build {
        env.push((
            "SCCACHE_RUST_INCREMENTAL_FIRST_BUILD".into(),
            first_build.into(),
        ));
    }
    restart_sccache(test, Some(env))
}

fn incremental_chain_checkouts(client_side: bool) -> Result<(SccacheTest<'static>, [PathBuf; 4])> {
    let (test, checkouts) = incremental_checkouts("cache", client_side)?;
    for checkout in &checkouts {
        fs::create_dir_all(checkout.join("lb/src"))?;
        fs::write(
            checkout.join("Cargo.toml"),
            "[package]\nname = \"test-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [workspace]\nresolver = \"2\"\nmembers = [\"la\", \"lb\"]\n\
             [lib]\nname = \"mylib\"\n\
             [[bin]]\nname = \"mybin\"\npath = \"src/bin.rs\"\n\
             [dependencies.lb]\npath = \"lb\"\n",
        )?;
        fs::write(
            checkout.join("lb/Cargo.toml"),
            "[package]\nname = \"lb\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies.la]\npath = \"../la\"\n",
        )?;
        fs::write(
            checkout.join("lb/src/lib.rs"),
            "pub fn answer() -> u32 { la::answer() + 100 }\n",
        )?;
        fs::write(
            checkout.join("src/lib.rs"),
            "pub fn answer() -> u32 { lb::answer() }\n",
        )?;
    }
    Ok((test, checkouts))
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
        .env_remove("SCCACHE_ALLOW_INCREMENTAL")
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

#[test]
#[serial]
fn test_rust_cargo_incremental_prunes_old_session_after_lockfile_change() -> Result<()> {
    let (test, checkouts) = incremental_checkouts("cache-no-bypass", false)?;
    restart_incremental_server(&test, &checkouts, "cache-no-bypass", Some("incremental"))?;
    let [a, ..] = checkouts;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    let before = fs::read(a.join("Cargo.lock"))?;
    let dirs = private_incremental_dirs(&a, "la")?;
    assert_eq!(dirs.len(), 1);
    let stale = &dirs[0];
    let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    for entry in walkdir::WalkDir::new(stale) {
        filetime::set_file_mtime(entry?.path(), old)?;
    }
    filetime::set_file_mtime(stale.with_file_name("la-sccache-prune"), old)?;

    let manifest = a.join("la/Cargo.toml");
    let updated = fs::read_to_string(&manifest)?.replace("0.1.0", "0.2.0");
    fs::write(&manifest, updated)?;
    incremental_cargo(&test, &a, "build")
        .assert()
        .try_success()?;
    assert_ne!(before, fs::read(a.join("Cargo.lock"))?);
    assert!(!stale.exists());
    let dirs = private_incremental_dirs(&a, "la")?;
    assert_eq!(dirs.len(), 1);
    assert_ne!(&dirs[0], stale);
    run_incremental_bin(&a, 10)
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

#[test_case(true, false, false; "server unhandled")]
#[test_case(true, true, false; "client side unhandled")]
#[test_case(false, false, false; "server unreachable")]
#[test_case(true, false, true; "incremental first build")]
#[serial]
fn test_rust_cargo_incremental_uncached_first_build(
    server: bool,
    client_side: bool,
    incremental_first: bool,
) -> Result<()> {
    let test = SccacheTest::new(None)?;
    let checkout = real_tempdir(&test)?.join("local");
    fs::create_dir_all(checkout.join("src"))?;
    fs::create_dir_all(checkout.join("tests"))?;
    fs::write(
        checkout.join("Cargo.toml"),
        "[package]\nname = 'local-bin'\nversion = '0.1.0'\nedition = '2021'\n\
         [[bin]]\nname = 'local_bin'\npath = 'src/main.rs'\ntest = false\n",
    )?;
    fs::write(
        checkout.join("src/main.rs"),
        r#"fn main() { println!("bin output"); }"#,
    )?;
    let config = checkout.join("sccache.toml");
    let mut settings = String::from("rust_incremental = 'cache'\nrust_cache_linked = 'off'\n");
    if incremental_first {
        settings.push_str("rust_incremental_first_build = 'incremental'\n");
    }
    settings.push_str("[server]\nautostart = false\nconnect_wait_secs = 0\n");
    fs::write(&config, settings)?;
    if server {
        restart_sccache(
            &test,
            Some(vec![(
                "SCCACHE_CONF".into(),
                config.to_str().context("non-Unicode config path")?.into(),
            )]),
        )?;
    } else {
        stop_sccache()?;
    }

    let cargo = || {
        let mut cmd = incremental_cargo(&test, &checkout, "test");
        cmd.env("SCCACHE_CONF", &config)
            .env("SCCACHE_CLIENT_SIDE", if client_side { "1" } else { "0" })
            .env_remove("SCCACHE_RUST_INCREMENTAL")
            .env("CARGO_INCREMENTAL", "1");
        cmd
    };

    let mut previous: Option<(PathBuf, Vec<PathBuf>)> = None;
    for value in 0..3 {
        fs::write(
            checkout.join("tests/local_harness.rs"),
            format!(
                r#"
#[test]
fn reports_current_value() {{
    let bin = std::process::Command::new(env!("CARGO_BIN_EXE_local_bin"))
        .output().unwrap();
    assert!(bin.status.success());
    assert_eq!(bin.stdout, b"bin output\n");
    println!("harness value: {value}");
}}
"#
            ),
        )?;
        let build = cargo().arg("--no-run").assert().try_success()?;
        if !server {
            build.try_stderr(predicate::str::contains("could not connect to the server"))?;
        }
        cargo()
            .args(["--", "--nocapture"])
            .assert()
            .try_success()?
            .try_stdout(predicate::str::contains(format!("harness value: {value}")))?;

        let bin_dirs = incremental_crate_dirs(&checkout, "local_bin")?;
        assert_eq!(bin_dirs.len(), usize::from(incremental_first));
        let harness_dirs = incremental_crate_dirs(&checkout, "local_harness")?;
        if value == 0 && !incremental_first {
            assert!(harness_dirs.is_empty(), "{harness_dirs:?}");
            continue;
        }

        assert_eq!(harness_dirs.len(), 1, "{harness_dirs:?}");
        assert!(private_incremental_dirs(&checkout, "local_harness")?.is_empty());
        assert_no_working_sessions(&checkout, &["local_harness"])?;
        let sessions: Vec<_> = fs::read_dir(&harness_dirs[0])?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with("s-") && entry.path().is_dir()
            })
            .map(|entry| entry.path())
            .collect();
        assert!(!sessions.is_empty(), "{harness_dirs:?}");
        if let Some((dir, old_sessions)) = &previous {
            assert_eq!(&harness_dirs[0], dir);
            assert!(
                sessions
                    .iter()
                    .any(|session| !old_sessions.contains(session))
            );
        }
        previous = Some((harness_dirs[0].clone(), sessions));
    }
    Ok(())
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

#[test_case("off", false, false ; "off")]
#[test_case("check", false, false ; "check")]
#[test_case("check", true, false ; "incremental check")]
#[test_case("check", true, true ; "client side incremental check")]
#[serial]
fn test_rust_cargo_linked_check(stage: &str, incremental: bool, client_side: bool) -> Result<()> {
    let test = SccacheTest::new(Some(&[(
        "SCCACHE_CLIENT_SIDE",
        if client_side { "1" } else { "0" }.into(),
    )]))?;
    let root = real_tempdir(&test)?;
    let [a, b] = ["a", "b"].map(|name| root.join(name));
    let incremental_mode = if incremental { "cache" } else { "off" };
    restart_sccache(
        &test,
        Some(vec![
            ("SCCACHE_RUST_CACHE_LINKED".into(), stage.into()),
            ("SCCACHE_RUST_INCREMENTAL".into(), incremental_mode.into()),
        ]),
    )?;
    let cargo = |checkout: &Path, cmd| {
        let mut cargo = incremental_cargo(&test, checkout, cmd);
        cargo
            .env("SCCACHE_BASEDIRS", checkout)
            .env("SCCACHE_RUST_CACHE_LINKED", "off")
            .env("CARGO_INCREMENTAL", if incremental { "1" } else { "0" });
        cargo
    };

    let cacheable = if stage == "off" { 1 } else { 5 };
    for (checkout, hits, misses) in [(&a, 0, cacheable), (&b, cacheable, 0)] {
        fs::create_dir_all(checkout.join("src"))?;
        fs::create_dir_all(checkout.join("tests"))?;
        fs::write(
            checkout.join("Cargo.toml"),
            "[package]\nname = 'linked-check'\nversion = '0.1.0'\nedition = '2021'\n\
             [workspace]\n[lib]\nname = 'linked_lib'\n\
             [[bin]]\nname = 'linked_bin'\npath = 'src/main.rs'\n\
             [profile.dev]\nsplit-debuginfo = 'off'\n",
        )?;
        fs::write(
            checkout.join("src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n\
             #[test] fn unit_test() { assert_eq!(answer(), 42); }\n",
        )?;
        fs::write(
            checkout.join("src/main.rs"),
            "fn main() { println!(\"{}\", linked_lib::answer()); }\n",
        )?;
        fs::write(
            checkout.join("tests/integration.rs"),
            "#[test] fn integration_test() { assert_eq!(linked_lib::answer(), 42); }\n",
        )?;

        zero_incremental_stats()?;
        cargo(checkout, "check")
            .arg("--all-targets")
            .assert()
            .try_success()?;
        let stats = incremental_stats(&test, hits, misses)?;
        assert_eq!(stats["cache_writes"], misses);
        for (reason, refused) in [("crate-type", 3), ("crate-type (bin)", 1)] {
            assert_eq!(
                stats["not_cached"][reason].as_u64().unwrap_or_default(),
                if stage == "off" { refused } else { 0 },
                "{stats}"
            );
        }
        if incremental {
            for name in ["linked_lib", "linked_bin", "integration"] {
                assert!(incremental_crate_dirs(checkout, name)?.is_empty());
            }
        }
    }

    let mut empty_metadata = 0;
    for entry in fs::read_dir(b.join("target/debug/deps"))? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "rmeta")
            && entry.metadata()?.len() == 0
        {
            empty_metadata += 1;
        }
    }
    assert_eq!(empty_metadata, 4);
    if stage != "off" {
        if incremental {
            for (edit, misses) in [(1, 1), (2, 0)] {
                fs::write(
                    b.join("src/main.rs"),
                    format!(
                        "// edit {edit}\nfn main() {{ println!(\"{{}}\", linked_lib::answer()); }}\n"
                    ),
                )?;
                zero_incremental_stats()?;
                let mut check = cargo(&b, "check");
                if client_side {
                    check
                        .env("SCCACHE_RUST_INCREMENTAL", "cache")
                        .env("SCCACHE_RUST_CACHE_LINKED", "check");
                }
                check.args(["--bin", "linked_bin"]).assert().try_success()?;
                let stats = incremental_stats(&test, 0, misses)?;
                assert_eq!(stats["cache_writes"], misses);
                assert_eq!(
                    stats["not_cached"]["incremental session"]
                        .as_u64()
                        .unwrap_or_default(),
                    u64::from(edit == 2 && !client_side),
                    "{stats}"
                );
                if edit == 2 && client_side {
                    assert_eq!(stats["compile_requests"], 0, "{stats}");
                }
                assert_eq!(private_incremental_dirs(&b, "linked_bin")?.len(), 1);
            }
        }
        cargo(&b, "test")
            .assert()
            .try_success()?
            .try_stdout(predicate::str::contains("test unit_test ... ok"))?
            .try_stdout(predicate::str::contains("test integration_test ... ok"))?;
        cargo(&b, "run")
            .assert()
            .try_success()?
            .try_stdout("42\n")?;
    }
    Ok(())
}

#[test]
#[serial]
fn test_rust_metadata_only_proc_macro() -> Result<()> {
    let test = SccacheTest::new(None)?;
    let root = real_tempdir(&test)?;
    let config = root.join("sccache.toml");
    fs::write(&config, "rust_cache_linked = 'proc-macro'\n")?;
    stop_sccache()?;
    Command::new(SCCACHE_BIN.as_os_str())
        .arg("--start-server")
        .env("SCCACHE_DIR", root.join("cache"))
        .env("SCCACHE_CONF", &config)
        .env_remove("SCCACHE_RUST_CACHE_LINKED")
        .assert()
        .try_success()?
        .try_stderr(predicate::str::contains("not implemented yet").not())?;

    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let [a, b] = ["a", "b"].map(|name| root.join(name));
    for dir in [&a, &b] {
        fs::create_dir_all(dir.join("out"))?;
        fs::write(
            dir.join("identity.rs"),
            "extern crate proc_macro;\n\
             #[proc_macro] pub fn identity(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n",
        )?;
        Command::new(SCCACHE_BIN.as_os_str())
            .arg(&rustc)
            .args([
                "--crate-name=identity",
                "--crate-type=proc-macro",
                "--emit=dep-info,metadata",
                "--extern",
                "proc_macro",
                "--out-dir=out",
                "identity.rs",
            ])
            .current_dir(dir)
            .env("SCCACHE_BASEDIRS", dir)
            .env("SCCACHE_RUST_CACHE_LINKED", "off")
            .assert()
            .try_success()?;
        assert!(dir.join("out/identity.d").is_file());
    }
    incremental_stats(&test, 1, 1)?;
    let original = fs::read(a.join("out/libidentity.rmeta"))?;
    assert!(!original.is_empty());
    assert_eq!(fs::read(b.join("out/libidentity.rmeta"))?, original);
    Ok(())
}

#[test_case(None ; "default")]
#[test_case(Some("1") ; "upstream allow")]
#[serial]
fn test_rust_cargo_incremental_off(allow: Option<&str>) -> Result<()> {
    let (test, [a, b, ..]) = incremental_checkouts("off", false)?;
    for (i, checkout) in [&a, &b].into_iter().enumerate() {
        if i == 1 {
            zero_incremental_stats()?;
        }
        let mut cargo = incremental_cargo(&test, checkout, "build");
        cargo.env("CARGO_INCREMENTAL", "1");
        if let Some(allow) = allow {
            cargo.env("SCCACHE_ALLOW_INCREMENTAL", allow);
        }
        cargo.assert().try_success()?;
        assert!(!incremental_crate_dirs(checkout, "mylib")?.is_empty());
    }
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

    restart_incremental_server(&test, &checkouts, "cache", None)?;
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
    let (test, checkouts) = incremental_checkouts("cache", client_side)?;
    restart_incremental_server(&test, &checkouts, "cache", Some("incremental"))?;
    let [a, ..] = checkouts;
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

    let stale = private[0].with_file_name("mylib-01abc3ef4567z");
    fs::create_dir(&stale)?;
    let old_mtime = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_mtime(&stale, old_mtime)?;
    filetime::set_file_mtime(stale.with_file_name("mylib-sccache-prune"), old_mtime)?;

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
    assert!(!stale.exists());
    assert_eq!(private_incremental_dirs(&a, "mylib")?, private);
    assert_ne!(incremental_sessions(&a, "mylib")?, old);
    run_incremental_bin(&a, 11)
}

#[test]
#[serial]
fn test_rust_cargo_incremental_plain_first_build_then_rebuild_and_bypass() -> Result<()> {
    let (test, [a, b, ..]) = incremental_chain_checkouts(false)?;
    let crates = &["la", "lb", "mylib"];
    let build = |checkout| {
        let mut cargo = incremental_cargo(&test, checkout, "build");
        cargo
            .arg("--workspace")
            .env("SCCACHE_RUST_INCREMENTAL", "cache")
            .env("SCCACHE_BASEDIRS", checkout);
        cargo
    };

    build(&a).assert().try_success()?;
    assert_eq!(incremental_stats(&test, 0, 3)?["cache_writes"], 3);
    assert_cache_only_sessions(&a, crates)?;
    run_incremental_bin(&a, 110)?;

    zero_incremental_stats()?;
    build(&b).assert().try_success()?;
    assert_eq!(incremental_stats(&test, 3, 0)?["cache_writes"], 0);
    assert_cache_only_sessions(&b, crates)?;
    run_incremental_bin(&b, 110)?;

    zero_incremental_stats()?;
    edit_la(&a, 20)?;
    build(&a).assert().try_success()?;
    assert_eq!(incremental_stats(&test, 0, 3)?["cache_writes"], 3);
    let sessions = crates
        .iter()
        .map(|name| incremental_sessions(&a, name))
        .collect::<Result<Vec<_>>>()?;
    run_incremental_bin(&a, 120)?;

    zero_incremental_stats()?;
    edit_la(&a, 30)?;
    build(&a).assert().try_success()?;
    let stats = incremental_stats(&test, 0, 0)?;
    // Only the binary went to the server. All three libraries bypassed locally.
    assert_eq!(stats["compile_requests"], 1, "{stats}");
    assert_eq!(stats["requests_executed"], 0, "{stats}");
    assert_eq!(stats["cache_writes"], 0, "{stats}");
    assert!(stats["not_cached"]["incremental session"].is_null());
    for (name, old) in crates.iter().zip(sessions) {
        assert_ne!(incremental_sessions(&a, name)?, old);
    }
    run_incremental_bin(&a, 130)
}

#[test_case(false ; "legacy cache")]
#[test_case(true ; "partially mixed cache")]
#[serial]
fn test_rust_cargo_incremental_first_build_variants_share_cache(mixed: bool) -> Result<()> {
    let (test, checkouts) = incremental_chain_checkouts(true)?;
    let [a, b, c, ..] = &checkouts;
    let crates = &["la", "lb", "mylib"];
    restart_incremental_server(&test, &checkouts, "cache", Some("incremental"))?;
    let build = |checkout| {
        let mut cargo = incremental_cargo(&test, checkout, "build");
        cargo.env("SCCACHE_RUST_INCREMENTAL_FIRST_BUILD", "plain");
        cargo
    };

    if mixed {
        build(a).args(["-p", "la"]).assert().try_success()?;
        assert_eq!(incremental_stats(&test, 0, 1)?["cache_writes"], 1);
        incremental_sessions(a, "la")?;
    } else {
        build(a).arg("--workspace").assert().try_success()?;
        assert_eq!(incremental_stats(&test, 0, 3)?["cache_writes"], 3);
        for name in crates {
            incremental_sessions(a, name)?;
        }
        run_incremental_bin(a, 110)?;
    }

    restart_incremental_server(&test, &checkouts, "cache", None)?;
    build(b).arg("--workspace").assert().try_success()?;
    let (hits, misses) = if mixed { (1, 2) } else { (3, 0) };
    assert_eq!(
        incremental_stats(&test, hits, misses)?["cache_writes"],
        misses
    );
    assert_cache_only_sessions(b, crates)?;
    run_incremental_bin(b, 110)?;

    if mixed {
        zero_incremental_stats()?;
        build(c).arg("--workspace").assert().try_success()?;
        assert_eq!(incremental_stats(&test, 3, 0)?["cache_writes"], 0);
        assert_cache_only_sessions(c, crates)?;
        run_incremental_bin(c, 110)?;
    }
    Ok(())
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
    let (test, checkouts) = incremental_checkouts("cache", false)?;
    restart_incremental_server(&test, &checkouts, "cache", Some("incremental"))?;
    let [a, b, ..] = checkouts;
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

    let (test, [a, b, ..]) = incremental_checkouts("cache-no-bypass", false)?;
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

    // B has the original outputs from the cache and can build a cold session.
    edit_mylib(&b, 1)?;
    incremental_cargo(&test, &b, "build")
        .env("SCCACHE_NO_CACHE", "1")
        .assert()
        .try_success()?;
    assert_eq!(fs::read(&output)?, fs::read(incremental_rmeta(&b)?)?);
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
    // Seed the same dependency outputs before building the cold session in C.
    incremental_cargo(&test, &c, "build")
        .assert()
        .try_success()?;
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
    let (test, checkouts) = incremental_checkouts("cache-no-bypass", false)?;
    restart_incremental_server(&test, &checkouts, "cache-no-bypass", Some("incremental"))?;
    let [a, b, ..] = checkouts;
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

    // The same non-incremental invocation can come from either Cargo setting.
    zero_incremental_stats()?;
    incremental_cargo(&test, &c, "build")
        .env("CARGO_INCREMENTAL", "0")
        .assert()
        .try_success()?;
    incremental_stats(&test, 3, 0)?;

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

#[test_case(false ; "daemon")]
#[test_case(true ; "client side")]
#[serial]
fn test_rust_cargo_incremental_cached(client_side: bool) -> Result<()> {
    let (test, checkouts) = incremental_checkouts("cache", client_side)?;
    let [a, b, ..] = &checkouts;
    incremental_cargo(&test, a, "build")
        .env("CARGO_INCREMENTAL", "1")
        .assert()
        .try_success()?;
    assert_eq!(incremental_stats(&test, 0, 3)?["cache_writes"], 3);
    assert!(incremental_crate_dirs(a, "mylib")?.is_empty());

    restart_incremental_server(&test, &checkouts, "off", None)?;
    let build = || {
        let mut cargo = incremental_cargo(&test, b, "build");
        cargo
            .env("CARGO_INCREMENTAL", "1")
            .env("SCCACHE_ALLOW_INCREMENTAL", "cache");
        cargo
    };
    build().assert().try_success()?;
    let stats = incremental_stats(&test, 3, 0)?;
    assert_eq!(stats["cache_writes"], 0);
    assert!(stats["not_cached"]["incremental"].is_null());
    assert!(incremental_crate_dirs(b, "mylib")?.is_empty());
    run_incremental_bin(b, 10)?;

    zero_incremental_stats()?;
    edit_mylib(b, 1)?;
    build().assert().try_success()?;
    assert_eq!(incremental_stats(&test, 0, 1)?["cache_writes"], 1);
    let session = incremental_sessions(b, "mylib")?;

    zero_incremental_stats()?;
    edit_mylib(b, 2)?;
    build().assert().try_success()?;
    let stats = incremental_stats(&test, 0, 0)?;
    assert_eq!(stats["not_cached"]["incremental session"], 1);
    assert_eq!(stats["cache_writes"], 0);
    assert_ne!(incremental_sessions(b, "mylib")?, session);
    run_incremental_bin(b, 12)
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

    // Keep the environment fixed across target directories, including in `all` mode.
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

fn wait_for_miss_log(
    path: &Path,
    expected: usize,
    include: impl Fn(&serde_json::Value) -> bool,
) -> Result<Vec<serde_json::Value>> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let mut entries = Vec::new();
        for line in contents
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
        {
            let entry = serde_json::from_str(line)?;
            if include(&entry) {
                entries.push(entry);
            }
        }
        if entries.len() >= expected {
            return Ok(entries);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "expected {expected} entries in {}, got {}",
            path.display(),
            entries.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test_case(false ; "server side")]
#[test_case(true ; "client side requested")]
#[serial]
fn test_rust_cargo_explain_cross_worktree_misses(client_side: bool) -> Result<()> {
    use serde_json::{Value, json};

    let mut test = SccacheTest::new(Some(&[
        (
            "SCCACHE_CLIENT_SIDE",
            if client_side { "1" } else { "0" }.into(),
        ),
        ("SCCACHE_BASEDIRS_REMAP_PATHS", "1".into()),
    ]))?;
    let root = real_tempdir(&test)?;
    let [a, b] = ["a", "b"].map(|name| root.join(name));
    let log = root.join("misses.jsonl");
    let start = |test| {
        restart_sccache(
            test,
            Some(vec![(
                "SCCACHE_MISS_EXPLAIN".into(),
                log.to_str().unwrap().into(),
            )]),
        )
    };
    let package = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
    };
    for (checkout, value) in [(&a, 42), (&b, 43)] {
        for crate_dir in ["leaf", "middle", "."] {
            fs::create_dir_all(checkout.join(crate_dir).join("src"))?;
        }
        write_compile_basedirs_config(checkout)?;
        fs::write(
            checkout.join("Cargo.toml"),
            package("explain_top") + "\n[dependencies]\nexplain_middle = { path = \"middle\" }\n",
        )?;
        fs::write(
            checkout.join("src/lib.rs"),
            "pub const ANSWER: (&str, u32) = explain_middle::ANSWER;\n",
        )?;
        fs::write(
            checkout.join("middle/Cargo.toml"),
            package("explain_middle")
                + "\n[dependencies]\nleaf = { package = \"explain_leaf\", path = \"../leaf\" }\n",
        )?;
        fs::write(
            checkout.join("middle/src/lib.rs"),
            "pub const ANSWER: (&str, u32) = leaf::ANSWER;\n",
        )?;
        fs::write(
            checkout.join("leaf/Cargo.toml"),
            package("explain_leaf") + "\n[features]\ndefault = [\"tracing\"]\ntracing = []\n",
        )?;
        fs::write(
            checkout.join("leaf/src/lib.rs"),
            format!("pub const ANSWER: (&str, u32) = (env!(\"TEST_ENV_VAR\"), {value});\n"),
        )?;
    }

    start(&test)?;
    cargo_build(&test, &a)?;
    // Restart to verify the baselines survive a server restart.
    start(&test)?;
    let keyed_env_value = "explain-misses-private-env-value";
    test.env.push(("TEST_ENV_VAR", keyed_env_value.into()));
    cargo_build(&test, &b)?;

    let stats: Value = serde_json::from_slice(&test.show_stats()?.get_output().stdout)?;
    assert_eq!(
        stats["stats"]["miss_explain"],
        json!({
            "new": 0, "own-input": 1, "dependency": 2, "unchanged": 0,
        })
    );
    test.show_text_stats(false)?
        .try_stdout(predicate::str::contains(
            "Rust misses explained (own-input)",
        ))?;

    let entries = wait_for_miss_log(&log, 6, |_| true)?;
    assert_eq!(entries.len(), 6);
    assert!(!fs::read_to_string(&log)?.contains(keyed_env_value));
    let in_checkout = |checkout: &Path, name: &str| {
        entries
            .iter()
            .find(|entry| {
                entry["basedir"] == checkout.to_str().unwrap() && entry["crate_name"] == name
            })
            .unwrap()
    };
    for crate_name in ["explain_leaf", "explain_middle", "explain_top"] {
        let old = in_checkout(&a, crate_name);
        let new = in_checkout(&b, crate_name);
        assert_eq!(old["classification"], "new");
        assert_eq!(old["crate_name"], new["crate_name"]);
        assert_eq!(old["identity"], new["identity"]);
        assert_eq!(old["key"], new["closest_key"]);
        assert_eq!(old["basedir"], a.to_str().unwrap());
        assert_eq!(new["basedir"], b.to_str().unwrap());
        for entry in [old, new] {
            assert!(
                entry["out_dir"]
                    .as_str()
                    .unwrap()
                    .starts_with("target/debug/")
            );
            assert_eq!(entry["profile"], "dev");
            assert_eq!(entry["target_kind"], "lib");
            assert_eq!(entry["crate_types"], json!(["rlib"]));
            assert_eq!(entry["is_test"], false);
            let features = if crate_name == "explain_leaf" {
                json!(["default", "tracing"])
            } else {
                json!([])
            };
            assert_eq!(entry["features"], features);
        }
        let differences = new["differences"].as_array().unwrap();
        let names: Vec<_> = differences
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        match crate_name {
            "explain_leaf" => {
                assert_eq!(new["classification"], "own-input");
                assert_eq!(names, ["env:TEST_ENV_VAR", "source:leaf/src/lib.rs"]);
            }
            "explain_middle" => {
                assert_eq!(new["classification"], "dependency");
                assert_eq!(names, ["extern:leaf"]);
                assert!(
                    in_checkout(&b, "explain_leaf")["artifacts"]
                        .as_array()
                        .unwrap()
                        .contains(&differences[0]["artifact"])
                );
            }
            "explain_top" => {
                assert_eq!(new["classification"], "dependency");
                assert_eq!(names, ["extern:explain_middle"]);
            }
            name => panic!("unexpected crate {name}"),
        }
        for difference in differences {
            assert!(difference["previous"].is_string());
            assert!(difference["current"].is_string());
            assert_ne!(difference["previous"], difference["current"]);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test_case("rust-lld")]
#[test_case("mold")]
#[test_case("bfd")]
#[serial]
fn test_rust_cargo_linked_proc_macro(linker: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let driver = if linker == "rust-lld" { "cc" } else { "clang" };
    let fuse_ld = (linker != "rust-lld").then(|| format!("-Clink-arg=-fuse-ld={linker}"));
    let probe = tempfile::tempdir()?;
    let probe_source = probe.path().join("probe.rs");
    let probe_dylib = probe.path().join("libprobe.so");
    let depfile = probe.path().join("link.d");
    fs::write(
        &probe_source,
        "extern crate proc_macro; #[proc_macro] pub fn probe(x: proc_macro::TokenStream) -> proc_macro::TokenStream { x }",
    )?;
    let mut rustc_probe = Command::new(&rustc);
    rustc_probe
        .arg(&probe_source)
        .args(["--crate-type=proc-macro", "--out-dir"])
        .arg(probe.path())
        .arg(format!("-Clinker={driver}"))
        .args(fuse_ld.iter());
    let mut with_depfile = rustc_probe.output()?;
    if !with_depfile.status.success() {
        eprintln!(
            "skipping {linker}: {}",
            String::from_utf8_lossy(&with_depfile.stderr)
        );
        return Ok(());
    }
    let bytes = fs::read(&probe_dylib)?;
    with_depfile = rustc_probe
        .arg(format!(
            "-Clink-arg=-Wl,--dependency-file={}",
            depfile.display()
        ))
        .args(["--print", "link-args"])
        .output()?;
    if !with_depfile.status.success()
        || !depfile.is_file()
        || (linker == "rust-lld"
            && !String::from_utf8_lossy(&with_depfile.stdout).contains("/gcc-ld"))
    {
        eprintln!("skipping {linker}: GNU dependency files unavailable");
        return Ok(());
    }
    assert!(
        bytes == fs::read(&probe_dylib)?,
        "{linker} changed the dylib bytes"
    );

    let test = SccacheTest::new(None)?;
    let root = test.tempdir.path();
    let wrapper = root.join("linker");
    let write_wrapper = |revision| -> Result<()> {
        fs::write(
            &wrapper,
            format!("#!/bin/sh\n# revision {revision}\nexec {driver} \"$@\"\n"),
        )?;
        fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    };
    write_wrapper(1)?;
    let miss_log = root.join("misses.jsonl");
    restart_sccache(
        &test,
        Some(vec![
            (
                "SCCACHE_MISS_EXPLAIN".into(),
                miss_log.display().to_string(),
            ),
            ("SCCACHE_RUST_CACHE_LINKED".into(), "proc-macro".into()),
        ]),
    )?;

    let a = root.join("checkout-a");
    let b = root.join("checkout-b");
    for checkout in [&a, &b] {
        write_linked_proc_macro_workspace(checkout)?;
    }
    let rustflags = format!(
        "-Clinker={} {}",
        wrapper.display(),
        fuse_ld.as_deref().unwrap_or_default()
    );
    let cargo = |checkout: &Path| {
        let mut cmd = Command::new(CARGO.as_os_str());
        cmd.envs(test.env.iter().cloned())
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("LIBRARY_PATH")
            .env_remove("LD_LIBRARY_PATH")
            .env("SCCACHE_BASEDIRS", checkout)
            .env("RUSTFLAGS", &rustflags)
            .current_dir(checkout);
        cmd
    };
    let answer = |checkout: &Path, value| -> Result<()> {
        cargo(checkout)
            .args(["run", "--offline", "-p", "user"])
            .assert()
            .try_success()?
            .try_stdout(format!("{value}\n"))?;
        Ok(())
    };
    let dylib = |checkout: &Path| -> Result<PathBuf> {
        for entry in fs::read_dir(checkout.join("target/debug/deps"))? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("libpm-")
                && entry.path().extension().is_some_and(|ext| ext == "so")
            {
                return Ok(entry.path());
            }
        }
        bail!("missing proc-macro output in {}", checkout.display());
    };

    answer(&a, 42)?;
    incremental_stats(&test, 0, 4)?;
    zero_incremental_stats()?;
    answer(&b, 42)?;
    incremental_stats(&test, 4, 0)?;
    assert!(
        fs::read(dylib(&a)?)? == fs::read(dylib(&b)?)?,
        "restored proc-macro differs"
    );

    fs::write(a.join("nat/answer.c"), "int answer(void) { return 43; }\n")?;
    zero_incremental_stats()?;
    answer(&a, 43)?;
    let pm_misses = |expected| -> Result<Vec<String>> {
        wait_for_miss_log(&miss_log, expected, |miss| miss["crate_name"] == "pm")?
            .into_iter()
            .map(|miss| {
                miss["key"]
                    .as_str()
                    .map(str::to_owned)
                    .context("missing link cache key")
            })
            .collect()
    };
    let misses = pm_misses(2)?;
    assert_eq!(misses.len(), 2);
    assert_eq!(
        misses[0], misses[1],
        "the C-only change must invalidate the manifest"
    );

    cargo(&a)
        .args(["clean", "-p", "pm"])
        .assert()
        .try_success()?;
    answer(&a, 43)?;
    assert_eq!(pm_misses(2)?.len(), 2, "the updated manifest should hit");

    let clean_pm = || -> Result<()> {
        cargo(&b)
            .args(["clean", "-p", "pm"])
            .assert()
            .try_success()?;
        zero_incremental_stats()
    };
    write_wrapper(2)?;
    clean_pm()?;
    cargo(&b)
        .args(["build", "--offline", "-p", "pm"])
        .assert()
        .try_success()?;
    incremental_stats(&test, 0, 1)?;
    assert_eq!(pm_misses(3)?.len(), 3);

    clean_pm()?;
    cargo(&b)
        .args([
            "rustc",
            "--offline",
            "-p",
            "pm",
            "--",
            "-Clink-arg=-Wl,--build-id=none",
        ])
        .assert()
        .try_success()?;
    incremental_stats(&test, 0, 1)?;
    assert_eq!(pm_misses(4)?.len(), 4);

    clean_pm()?;
    cargo(&b)
        .args(["build", "--offline", "-p", "pm"])
        .env("LIBRARY_PATH", b.join("other-libraries"))
        .assert()
        .try_success()?;
    incremental_stats(&test, 0, 1)?;
    assert_eq!(pm_misses(5)?.len(), 5);

    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nfor arg do\n    case \"$arg\" in\n        -Wl,--dependency-file=*) shift ;;\n        *) shift; set -- \"$@\" \"$arg\" ;;\n    esac\ndone\nexec {driver} \"$@\"\n"
        ),
    )?;
    for expected in [6, 7] {
        clean_pm()?;
        cargo(&b)
            .args(["build", "--offline", "-p", "pm"])
            .assert()
            .try_success()?;
        incremental_stats(&test, 0, 0)?;
        assert_eq!(pm_misses(expected)?.len(), expected);
    }
    assert_failed_linked_proc_macro_runs_once(&test, &rustc, driver, fuse_ld.as_deref())?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_failed_linked_proc_macro_runs_once(
    test: &SccacheTest,
    rustc: &std::ffi::OsStr,
    driver: &str,
    fuse_ld: Option<&str>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let sysroot = Command::new(rustc).args(["--print", "sysroot"]).output()?;
    assert!(sysroot.status.success());
    let sysroot = String::from_utf8(sysroot.stdout)?;
    let rustc = Path::new(sysroot.trim()).join("bin/rustc");
    let root = test.tempdir.path();
    let wrapper = root.join("rustc");
    let runs = root.join("rustc-runs");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
codegen=0
print=0
capture=plain
for arg do
    case "$arg" in
        --emit=dep-info,link|dep-info,link) codegen=1 ;;
        --print|--print=*) print=1 ;;
        link-arg=*--dependency-file=*|-Clink-arg=*--dependency-file=*) capture=captured ;;
    esac
done
if [ "$codegen" = 1 ] && [ "$print" = 0 ]; then
    printf '%s\n' "$capture" >> "$LINK_TEST_RUNS"
    printf '%s compile\n' "$capture" >&2
fi
exec "$LINK_TEST_RUSTC" "$@"
"#,
    )?;
    fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    fs::create_dir(root.join("out"))?;
    fs::write(
        root.join("broken.rs"),
        r#"extern crate proc_macro;
#[proc_macro]
pub fn broken(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let _: u8 = "deliberate type error";
    input
}
"#,
    )?;
    let args = [
        "--crate-type=proc-macro",
        "--crate-name=broken",
        "--out-dir=out",
        "--emit=dep-info,link",
        "broken.rs",
    ];
    let linker = format!("-Clinker={driver}");
    let expected = Command::new(&rustc)
        .args(args)
        .arg(&linker)
        .args(fuse_ld)
        .current_dir(root)
        .output()?;
    let code = expected.status.code().context("rustc exited by signal")?;
    assert_ne!(code, 0);

    zero_incremental_stats()?;
    Command::new(SCCACHE_BIN.as_os_str())
        .arg(&wrapper)
        .args(args)
        .arg(&linker)
        .args(fuse_ld)
        .env("LINK_TEST_RUSTC", rustc)
        .env("LINK_TEST_RUNS", &runs)
        .env_remove("LD_LIBRARY_PATH")
        .current_dir(root)
        .assert()
        .try_code(code)?
        .try_stderr(predicate::str::contains("error[E0308]"))?
        .try_stderr(predicate::str::contains("captured compile"))?;
    assert_eq!(fs::read_to_string(runs)?, "captured\n");
    let stats = incremental_stats(test, 0, 0)?;
    assert_eq!(stats["compile_fails"], 1);
    assert_eq!(stats["cache_writes"], 0);
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_linked_proc_macro_workspace(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"nat\", \"mid\", \"pm\", \"user\"]\nresolver = \"2\"\n",
    )?;
    write_linked_native_crates(root)?;
    for (name, manifest, source) in [
        (
            "pm",
            "[lib]\nproc-macro = true\n[dependencies]\nmid = { path = \"../mid\" }\n",
            r#"
            use proc_macro::TokenStream;
            #[proc_macro]
            pub fn answer(_: TokenStream) -> TokenStream { mid::value().to_string().parse().unwrap() }
        "#,
        ),
        (
            "user",
            "[dependencies]\npm = { path = \"../pm\" }\n",
            "pub fn answer() -> i32 { pm::answer!() }",
        ),
    ] {
        linked::write_package(&root.join(name), name, manifest, &[("src/lib.rs", source)])?;
    }
    fs::write(
        root.join("user/src/main.rs"),
        "fn main() { println!(\"{}\", user::answer()); }",
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_linked_native_crates(root: &Path) -> Result<()> {
    for (name, manifest, source) in [
        (
            "nat",
            "",
            r#"
            unsafe extern "C" { fn answer() -> i32; }
            pub fn value() -> i32 { unsafe { answer() } }
        "#,
        ),
        (
            "mid",
            "[dependencies]\nnat = { path = \"../nat\" }\n",
            "pub fn value() -> i32 { nat::value() }",
        ),
    ] {
        linked::write_package(&root.join(name), name, manifest, &[("src/lib.rs", source)])?;
    }
    fs::write(
        root.join("nat/answer.c"),
        "int answer(void) { return 42; }\n",
    )?;
    fs::write(
        root.join("nat/build.rs"),
        r#"
        use std::{env, path::PathBuf, process::Command};
        fn main() {
            let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
            let object = out.join("answer.o");
            let archive = out.join("libanswer.a");
            assert!(Command::new("cc").args(["-c", "answer.c", "-o"]).arg(&object).status().unwrap().success());
            assert!(Command::new("ar").arg("crs").arg(archive).arg(object).status().unwrap().success());
            println!("cargo:rerun-if-changed=answer.c");
            let earlier = env::current_dir().unwrap().parent().unwrap().join("earlier");
            if earlier.is_dir() {
                println!("cargo:rustc-link-search=native={}", earlier.display());
            }
            println!("cargo:rustc-link-search=native={}", out.display());
            println!("cargo:rustc-link-lib=static=answer");
        }
    "#,
    )?;
    Ok(())
}
