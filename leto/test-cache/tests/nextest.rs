use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const RUNNER: &str = env!("CARGO_BIN_EXE_leto-test-runner");
const RUNNER_ENV: &str = "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER";

struct Scratch(PathBuf);

impl Scratch {
    fn create() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("leto-shadow-{}-{stamp}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir(root.join(".config")).unwrap();
        for path in [
            "Cargo.toml",
            "fixture.txt",
            "src/lib.rs",
            ".config/nextest.toml",
        ] {
            fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/toy")
                    .join(path),
                root.join(path),
            )
            .unwrap();
        }
        Self(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cargo(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove(RUNNER_ENV)
        .env_remove("TERM")
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env_remove("LETO_TEST_CACHE");
    command
}

fn harness(root: &Path) -> PathBuf {
    fs::read_dir(root.join("target/debug/deps"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension().is_none()
                && path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("shadow_toy-")
        })
        .unwrap()
}

fn configure(command: &mut Command, root: &Path, test: &str) {
    command
        .current_dir(root)
        .env_remove("LETO_TEST_CACHE")
        .env("LETO_TEST_CACHE_DIR", root.join("logs"))
        .env("LETO_TEST_CACHE_OBSERVE", "1")
        .env("NEXTEST_BINARY_ID", "shadow-toy")
        .env("CARGO_MANIFEST_DIR", root)
        .env("NEXTEST_WORKSPACE_ROOT", root)
        .env("NEXTEST_TEST_NAME", test);
}

fn records(root: &Path) -> Vec<Value> {
    fs::read_dir(root.join("logs/runs"))
        .unwrap()
        .flat_map(|file| {
            fs::read_to_string(file.unwrap().path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn details(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn direct_exec_and_shadow_preserve_output_exit_and_signals() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let build = cargo(root)
        .args(["test", "--no-run", "--offline"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", details(&build));
    let binary = harness(root);
    for test in ["tests::pure", "tests::explicit_exit", "tests::killed"] {
        let args = ["--exact", test, "--nocapture", "--include-ignored"];
        let plain = Command::new(&binary)
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        let mut shadow = Command::new(RUNNER);
        configure(&mut shadow, root, test);
        let observed = shadow.arg(&binary).args(args).output().unwrap();
        assert_eq!(plain.status, observed.status, "{}", details(&observed));
        assert_eq!(plain.stdout, observed.stdout);
        assert_eq!(plain.stderr, observed.stderr);
    }
    let output_with_input = |mut command: Command| {
        let mut child = command
            .args([
                "--exact",
                "tests::stdin",
                "--nocapture",
                "--include-ignored",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"stdin marker")
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let mut plain = Command::new(&binary);
    plain.current_dir(root);
    let mut shadow = Command::new(RUNNER);
    configure(&mut shadow, root, "tests::stdin");
    shadow.arg(&binary);
    let plain = output_with_input(plain);
    let shadow = output_with_input(shadow);
    assert!(plain.status.success());
    assert_eq!(plain.status, shadow.status);
    assert_eq!(plain.stdout, shadow.stdout);
    assert_eq!(plain.stderr, shadow.stderr);
    assert_eq!(records(root).len(), 4);
    let mut command = Command::new(RUNNER);
    configure(&mut command, root, "tests::relay");
    let mut child = command
        .arg(&binary)
        .args([
            "--exact",
            "tests::relay",
            "--nocapture",
            "--include-ignored",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("relay ready") {
        line.clear();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0);
    }
    // SAFETY: this deliberately signals our runner child only.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
    drop(stdout);

    for (kind, args) in [
        ("off", vec!["--exact", "tests::pure", "--nocapture"]),
        ("list", vec!["--list", "--format", "terse", "--ignored"]),
        ("no-nextest", vec!["--exact", "tests::pure", "--nocapture"]),
        ("not-test", vec!["ordinary", "program"]),
        ("bad-store", vec!["--exact", "tests::pure", "--nocapture"]),
    ] {
        let executable = if kind == "not-test" {
            Path::new("/bin/echo")
        } else {
            &binary
        };
        let plain = Command::new(executable)
            .current_dir(root)
            .args(&args)
            .output()
            .unwrap();
        let mut command = Command::new(RUNNER);
        configure(&mut command, root, "tests::pure");
        match kind {
            "off" => {
                command.env("LETO_TEST_CACHE", "off");
            }
            "no-nextest" => {
                command
                    .env_remove("NEXTEST_BINARY_ID")
                    .env_remove("NEXTEST_TEST_NAME");
            }
            "bad-store" => {
                command
                    .env("LETO_TEST_CACHE_DIR", root.join("fixture.txt"))
                    .env("HOME", root);
            }
            _ => {}
        }
        let shadow = command.arg(executable).args(args).output().unwrap();
        assert_eq!(plain.status, shadow.status);
        assert_eq!(plain.stdout, shadow.stdout);
        assert_eq!(plain.stderr, shadow.stderr);
    }
    assert_eq!(records(root).len(), 5);
    assert!(root.join(".cache/leto-test-results/v0/errors").is_dir());
}

fn reject_syscall(command: &mut Command, syscall: libc::c_long, option: Option<i32>) {
    let statement = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let equals = |k, jf| libc::sock_filter {
        code: 0x15,
        jt: 0,
        jf,
        k,
    };
    let mut code = vec![statement(0x20, 0)];
    if let Some(option) = option {
        code.extend([
            equals(syscall as u32, 3),
            statement(0x20, 16),
            equals(option as u32, 1),
        ]);
    } else {
        code.push(equals(syscall as u32, 1));
    }
    code.extend([
        statement(0x06, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
        statement(0x06, libc::SECCOMP_RET_ALLOW),
    ]);
    // SAFETY: the hook passes initialized filter data to libc. It runs
    // only in the command subprocess before exec.
    unsafe {
        command.pre_exec(move || {
            let filter = code.as_ptr().cast_mut();
            let program = libc::sock_fprog {
                len: code.len() as u16,
                filter,
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1
                || libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[test]
fn unavailable_observer_runs_the_test_without_observation() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let build = cargo(root)
        .args(["test", "--no-run", "--offline"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", details(&build));
    let binary = harness(root);
    let args = ["--exact", "tests::pure", "--nocapture"];
    let plain = Command::new(&binary)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    for (syscall, option) in [
        (libc::SYS_ptrace, None),
        (libc::SYS_prctl, Some(libc::PR_SET_SECCOMP)),
    ] {
        let mut shadow = Command::new(RUNNER);
        configure(&mut shadow, root, "tests::pure");
        shadow.arg(&binary).args(args);
        reject_syscall(&mut shadow, syscall, option);
        let shadow = shadow.output().unwrap();
        assert_eq!(plain.status, shadow.status);
        assert_eq!(plain.stdout, shadow.stdout);
        assert_eq!(plain.stderr, shadow.stderr);
    }
    let records = records(root);
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record["observer"] == "unavailable")
    );
}

#[test]
fn nextest_classifies_tests_and_preserves_timeout_and_listing() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let outside = Scratch::create();
    let temp = outside.0.join("tmp");
    let outside_file = outside.0.join("outside.txt");
    fs::create_dir(&temp).unwrap();
    fs::write(&outside_file, "outside\n").unwrap();
    let version = cargo(root).args(["nextest", "--version"]).output().unwrap();
    if !version.status.success() {
        eprintln!("skipped nextest integration: cargo-nextest is unavailable");
        return;
    }
    let run = |with_runner, args: &[&str]| {
        let mut command = cargo(root);
        if with_runner {
            command
                .env(RUNNER_ENV, RUNNER)
                .env("LETO_TEST_CACHE_DIR", root.join("logs"))
                .env("LETO_TEST_CACHE_OBSERVE", "1");
        }
        command.env("TMPDIR", &temp).args(args).output().unwrap()
    };
    let list = ["nextest", "list", "--message-format", "json", "--offline"];
    let plain_list = run(false, &list);
    let shadow_list = run(true, &list);
    assert!(plain_list.status.success(), "{}", details(&plain_list));
    assert_eq!(plain_list.stdout, shadow_list.stdout);
    assert!(!root.join("logs").exists());

    let args = [
        "nextest",
        "run",
        "--profile",
        "shadow",
        "--no-fail-fast",
        "--offline",
        "--color",
        "never",
    ];
    let plain = run(false, &args);
    let shadow = run(true, &args);
    let plain_text = details(&plain);
    let shadow_text = details(&shadow);
    assert_eq!(plain.status, shadow.status, "{plain_text}\n{shadow_text}");
    assert!(!plain.status.success(), "{plain_text}");
    for snippet in [
        "TIMEOUT",
        "tests::timeout",
        "SIGTERM",
        "tests::killed",
        "8 passed",
    ] {
        assert!(
            plain_text.contains(snippet) && shadow_text.contains(snippet),
            "{snippet}\n{plain_text}\n{shadow_text}"
        );
    }
    assert!(!shadow_text.contains("LEAK"), "{shadow_text}");

    let records = records(root);
    assert_eq!(
        records.len(),
        10,
        "{shadow_text}\n{records:#?}\n{:?}",
        fs::read_dir(root.join("logs/errors")).map(|entries| entries
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>())
    );
    for (test, class) in [
        ("pure", "pure"),
        ("ancestor_lookups", "pure"),
        ("ancestor_listing", "other"),
        ("outside_read", "other"),
        ("files", "files"),
        ("network", "network"),
        ("exec", "exec"),
        ("other", "other"),
        ("killed", "pure"),
        ("timeout", "exec"),
    ] {
        let record = records
            .iter()
            .find(|row| row["test"] == format!("tests::{test}"))
            .unwrap();
        assert_eq!(record["observer"]["class"], class, "{record}");
        let contains_path = |field: &str, path: &Path| {
            let canonical = path.canonicalize().unwrap();
            assert!(
                record["observer"][field]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry.as_str() == canonical.to_str()),
                "{field} missing {} in {record}",
                canonical.display()
            );
        };
        match test {
            "ancestor_lookups" => {
                for path in [Path::new("/"), root.parent().unwrap(), &outside.0] {
                    contains_path("ancestor_lookups", path);
                }
            }
            "ancestor_listing" => contains_path("other_reads", root.parent().unwrap()),
            "outside_read" => contains_path("other_reads", &outside_file),
            "files" => {
                assert!(
                    record["observer"]["checkout_reads"]
                        .to_string()
                        .contains("fixture.txt"),
                    "{record}"
                );
                assert!(
                    record["observer"]["absent_probes"]
                        .to_string()
                        .contains("absent.txt"),
                    "{record}"
                );
            }
            "exec" => {
                assert!(
                    record["observer"]["executables"]
                        .to_string()
                        .contains("/bin/cat"),
                    "{record}"
                );
                assert!(
                    record["observer"]["checkout_reads"]
                        .to_string()
                        .contains("fixture.txt"),
                    "{record}"
                );
            }
            "network" => {
                assert!(
                    record["observer"]["sockets"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["family"] == 2 && entry["loopback"] == true),
                    "{record}"
                );
            }
            "other" => assert!(
                record["observer"]["outside_writes"]
                    .to_string()
                    .contains("outside-tmp.txt"),
                "{record}"
            ),
            "killed" | "timeout" => {
                assert_eq!(record["outcome"]["signal"], libc::SIGTERM, "{record}")
            }
            _ => {}
        }
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

// AWS_* and ALCHEMY_* hold credentials on the hosts; keyed env values may only
// reach the store as part of the key hash.
#[test]
fn keyed_env_values_never_reach_records_errors_or_reports() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let build = cargo(root)
        .args(["test", "--no-run", "--offline"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", details(&build));
    let binary = harness(root);
    let canaries = ["AKIAcanary7f3e9d2b", "alchemy-canary-51c0ffee"];
    let run = |test: &str, secret: &str, observe: &str| {
        let mut shadow = Command::new(RUNNER);
        configure(&mut shadow, root, test);
        shadow
            .env("LETO_TEST_CACHE_OBSERVE", observe)
            .env("LETO_TEST_CACHE_ENV", "AWS_*,ALCHEMY_*")
            .env("AWS_SECRET_ACCESS_KEY", secret)
            .env("ALCHEMY_API_KEY", canaries[1])
            .arg(&binary)
            .args(["--exact", test, "--nocapture"])
            .output()
            .unwrap()
    };
    for test in ["tests::pure", "tests::files", "tests::exec"] {
        run(test, canaries[0], "1");
    }
    run("tests::pure", "AKIAsecond-value-0000", "1");
    // An invalid setting takes the error path, which logs the error text.
    run("tests::pure", canaries[0], "not-a-number");

    let records = records(root);
    let pure: Vec<_> = records
        .iter()
        .filter(|record| record["test"] == "tests::pure")
        .collect();
    assert_eq!(pure.len(), 2, "{records:?}");
    assert_ne!(
        pure[0]["key"], pure[1]["key"],
        "keyed values must change the key"
    );
    assert!(
        root.join("logs/errors").is_dir(),
        "the error path was not exercised"
    );

    let report = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("report.py"))
        .arg("--store")
        .arg(root.join("logs"))
        .arg("--output")
        .arg(root.join("report"))
        .output()
        .unwrap();
    assert!(report.status.success(), "{}", details(&report));

    let secret_test = "tests::prints_keyed_values";
    let gate = |mode| {
        stage_command(root, &binary, secret_test, mode, "secrets")
            .env("LETO_TEST_CACHE_ENV", "AWS_*,ALCHEMY_*")
            .env("AWS_SECRET_ACCESS_KEY", canaries[0])
            .env("ALCHEMY_API_KEY", canaries[1])
            .output()
            .unwrap()
    };
    let stored = gate("gate");
    assert!(stored.status.success(), "{}", details(&stored));
    assert_eq!(
        stage_records(root).last().unwrap()["cache"]["action"],
        "stored"
    );
    let replay = gate("replay");
    assert!(replay.status.success(), "{}", details(&replay));
    assert_eq!(
        stage_records(root).last().unwrap()["cache"]["action"],
        "replayed"
    );
    assert_eq!(replay.stdout, stored.stdout);
    assert_eq!(replay.stderr, stored.stderr);

    let mut checked = 0;
    for path in files_under(&root.join("logs"))
        .into_iter()
        .chain(files_under(&root.join("report")))
        .chain(files_under(&root.join("stage1")))
    {
        let content = fs::read(&path).unwrap();
        for canary in canaries.iter().chain(&["AKIAsecond-value-0000"]) {
            assert!(
                !content
                    .windows(canary.len())
                    .any(|w| w == canary.as_bytes()),
                "{canary} leaked into {}",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(
        checked >= 4,
        "only {checked} store and report files checked"
    );
}

fn stage_command(root: &Path, binary: &Path, test: &str, mode: &str, run: &str) -> Command {
    let mut command = Command::new(RUNNER);
    configure(&mut command, root, test);
    command
        .env("LETO_TEST_CACHE_DIR", root.join("stage1"))
        .env("LETO_TEST_CACHE", mode)
        .env("NEXTEST_ATTEMPT", "1")
        .env("NEXTEST_RUN_ID", run)
        .env_remove("TOY_UNKEYED")
        .arg(binary)
        .args(["--exact", test, "--nocapture"]);
    if matches!(
        test,
        "tests::private_temp"
            | "tests::explicit_exit"
            | "tests::swallowed_denial"
            | "tests::metadata_only"
            | "tests::verification"
            | "tests::verification_signal"
            | "tests::prints_keyed_values"
    ) {
        command.arg("--ignored");
    }
    command
}

fn stage_records(root: &Path) -> Vec<Value> {
    files_under(&root.join("stage1/runs"))
        .iter()
        .flat_map(|file| {
            fs::read_to_string(file)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn stage_entry(root: &Path, record: &Value) -> PathBuf {
    let key = record["key"].as_str().unwrap();
    root.join("stage1").join(&key[..2]).join(key)
}

#[test]
fn stage_one_stores_replays_expires_and_quarantines() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let build = cargo(root)
        .args(["test", "--no-run", "--offline"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", details(&build));
    let binary = harness(root);
    let run = |test, mode, id| {
        stage_command(root, &binary, test, mode, id)
            .output()
            .unwrap()
    };
    let pure = "tests::pure";

    let miss = run(pure, "replay", "empty");
    assert!(miss.status.success(), "{}", details(&miss));
    let last = || stage_records(root).pop().unwrap();
    assert_eq!(last()["cache"]["action"], "miss");
    assert!(!root.join("stage1/closures").exists());
    assert!(!stage_entry(root, &last()).exists());

    let stored = run(pure, "gate", "fill");
    assert!(stored.status.success(), "{}", details(&stored));
    assert_eq!(last()["cache"]["action"], "stored", "{}", last());
    let path = stage_entry(root, &last());
    let saved = fs::read(&path).unwrap();
    assert_eq!(last()["observer"]["denied"], false);
    let replay = run(pure, "replay", "read");
    assert!(replay.status.success(), "{}", details(&replay));
    assert_eq!(replay.stdout, stored.stdout);
    assert_eq!(replay.stderr, stored.stderr);
    assert_eq!(last()["cache"]["action"], "replayed");
    assert_eq!(fs::read(&path).unwrap(), saved);
    let verified = run(pure, "gate", "verify");
    assert!(verified.status.success(), "{}", details(&verified));
    assert_eq!(last()["cache"]["verification"], "matched");

    fs::File::open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86_400))
        .unwrap();
    assert!(run(pure, "replay", "expired").status.success());
    assert_eq!(last()["cache"]["action"], "miss");
    assert_eq!(fs::read(path).unwrap(), saved);

    for test in [
        "tests::files",
        "tests::network",
        "tests::exec",
        "tests::swallowed_denial",
        "tests::other",
    ] {
        let impure = run(test, "gate", "fill");
        assert!(impure.status.success(), "{}", details(&impure));
        let row = last();
        assert_eq!(row["cache"]["action"], "impure", "{row}");
        assert_eq!(row["observer"]["denied"], true, "{row}");
        let marker = fs::read_to_string(stage_entry(root, &row)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&marker).unwrap()["status"],
            "impure"
        );
        let replay = run(test, "replay", "read");
        assert!(replay.status.success(), "{}", details(&replay));
        assert_eq!(last()["cache"]["action"], "impure");
    }

    assert!(run("tests::metadata_only", "gate", "fill").status.success());
    assert_eq!(last()["cache"]["action"], "impure", "{}", last());
    assert_eq!(last()["observer"]["class"], "files");
    assert_eq!(last()["observer"]["denied"], false);
    assert!(stage_entry(root, &last()).exists());

    let temp = stage_command(root, &binary, "tests::private_temp", "gate", "fill")
        .env("TOY_UNKEYED", "never inherited")
        .output()
        .unwrap();
    assert!(temp.status.success(), "{}", details(&temp));
    assert_eq!(last()["cache"]["action"], "stored", "{}", last());

    let failure = run("tests::explicit_exit", "gate", "fill");
    assert_eq!(failure.status.code(), Some(37), "{}", details(&failure));
    assert!(
        failure
            .stdout
            .windows(b"exit stdout".len())
            .any(|w| w == b"exit stdout")
    );
    assert!(!stage_entry(root, &last()).exists());

    let test = "tests::verification";
    assert!(run(test, "gate", "fill").status.success());
    assert_eq!(last()["cache"]["action"], "stored", "{}", last());
    let path = stage_entry(root, &last());
    let mismatch = stage_command(root, &binary, test, "gate", "mismatch")
        .env("TOY_UNKEYED", "uncached output")
        .output()
        .unwrap();
    assert_eq!(mismatch.status.code(), Some(125), "{}", details(&mismatch));
    assert!(details(&mismatch).contains("verification failed"));
    assert_eq!(last()["cache"]["verification"], "mismatch");
    assert_eq!(last()["cache"]["quarantined"], true);
    assert!(!path.exists());
    assert_eq!(files_under(&root.join("stage1/quarantine")).len(), 1);

    let retry = stage_command(root, &binary, test, "gate", "mismatch")
        .env("NEXTEST_ATTEMPT", "2")
        .output()
        .unwrap();
    assert_eq!(retry.status.code(), Some(125), "{}", details(&retry));
    for mode in ["replay", "gate"] {
        assert!(run(test, mode, "different-run").status.success());
        assert_eq!(last()["cache"]["action"], "quarantined");
        assert!(!path.exists());
    }

    let test = "tests::verification_signal";
    assert!(run(test, "gate", "fill").status.success());
    assert_eq!(last()["cache"]["action"], "stored");
    let signaled = stage_command(root, &binary, test, "gate", "signaled")
        .env("TOY_UNKEYED", "terminate")
        .output()
        .unwrap();
    assert_eq!(signaled.status.code(), Some(125), "{}", details(&signaled));
    assert_eq!(last()["cache"]["verification"], "mismatch");
    assert_eq!(last()["cache"]["quarantined"], true);
}

#[test]
fn nextest_gate_and_replay_use_the_real_target_runner() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let available = cargo(root).args(["nextest", "--version"]).output().unwrap();
    if !available.status.success() {
        eprintln!("SKIP: cargo-nextest is unavailable");
        return;
    }
    for (mode, action) in [
        ("gate", "stored"),
        ("replay", "replayed"),
        ("gate", "verified"),
    ] {
        let output = cargo(root)
            .env(RUNNER_ENV, RUNNER)
            .env("LETO_TEST_CACHE", mode)
            .env("LETO_TEST_CACHE_DIR", root.join("stage1"))
            .args([
                "nextest",
                "run",
                "--offline",
                "--profile",
                "shadow",
                "-E",
                "test(=tests::pure)",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", details(&output));
        let row = stage_records(root).pop().unwrap();
        assert_eq!(
            row["cache"]["action"],
            action,
            "{row}\n{}",
            details(&output)
        );
    }
}

#[test]
fn gate_refuses_to_store_without_landlock_and_replay_still_works() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let build = cargo(root)
        .args(["test", "--no-run", "--offline"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", details(&build));
    let binary = harness(root);
    let test = "tests::pure";
    let mut gate = stage_command(root, &binary, test, "gate", "no-landlock");
    reject_syscall(&mut gate, libc::SYS_landlock_create_ruleset, None);
    let unavailable = gate.output().unwrap();
    assert!(unavailable.status.success(), "{}", details(&unavailable));
    assert!(details(&unavailable).contains("Landlock unavailable"));
    let row = stage_records(root).pop().unwrap();
    assert_eq!(row["cache"]["action"], "unavailable");
    assert!(!stage_entry(root, &row).exists());

    let saved = stage_command(root, &binary, test, "gate", "landlock")
        .output()
        .unwrap();
    assert!(saved.status.success(), "{}", details(&saved));
    assert_eq!(
        stage_records(root).pop().unwrap()["cache"]["action"],
        "stored"
    );
    let mut replay = stage_command(root, &binary, test, "replay", "no-landlock");
    reject_syscall(&mut replay, libc::SYS_landlock_create_ruleset, None);
    let replayed = replay.output().unwrap();
    assert_eq!(
        stage_records(root).pop().unwrap()["cache"]["action"],
        "replayed"
    );
    assert_eq!(replayed.stdout, saved.stdout);
    assert_eq!(replayed.stderr, saved.stderr);
}

#[test]
fn shadow_hashes_ld_preload_by_content_digest_and_allows_preload_reads() {
    let scratch = Scratch::create();
    let root = &scratch.0;
    let build = cargo(root)
        .args(["test", "--no-run", "--offline"])
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", details(&build));
    let binary = harness(root);
    let test = "tests::pure";

    let compile_shim = |dir: &Path, body: &str| -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let src = dir.join("shim.c");
        let so = dir.join("shim.so");
        fs::write(&src, body).unwrap();
        let out = Command::new("cc")
            .args(["-shared", "-fPIC", "-o"])
            .arg(&so)
            .arg(&src)
            .arg("-ldl")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", details(&out));
        so
    };

    // Place the preload objects outside both the checkout (`root`) and `TMPDIR`
    // so the test proves the observer allows preloaded objects via `libraries`.
    let preload_root = Scratch::create();
    let test_tmp = root.join("test-tmp");
    fs::create_dir_all(&test_tmp).unwrap();
    let dir_a = preload_root.0.join("preload-a");
    let dir_b = preload_root.0.join("preload-b");
    let dir_c = preload_root.0.join("preload-c");
    let so_a = compile_shim(&dir_a, "int leto_shim_v(void) { return 1; }\n");
    let so_b = compile_shim(&dir_b, "int leto_shim_v(void) { return 1; }\n");
    let so_c = compile_shim(
        &dir_c,
        "#include <unistd.h>\nint leto_shim_v(void) { return (int)getpid(); }\n",
    );
    assert_eq!(fs::read(&so_a).unwrap(), fs::read(&so_b).unwrap());
    assert_ne!(fs::read(&so_a).unwrap(), fs::read(&so_c).unwrap());

    let run_shadow = |preload: Option<&Path>| {
        let mut cmd = Command::new(RUNNER);
        configure(&mut cmd, root, test);
        cmd.env_remove("TERM")
            .env_remove("LD_PRELOAD")
            .env("TMPDIR", &test_tmp);
        if let Some(so) = preload {
            cmd.env("LD_PRELOAD", so);
        }
        cmd.arg(&binary)
            .args(["--exact", test, "--nocapture", "--include-ignored"]);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", details(&out));
    };

    run_shadow(None);
    run_shadow(Some(&so_a));
    run_shadow(Some(&so_b));
    run_shadow(Some(&so_c));

    let rows = records(root);
    let errs = fs::read_dir(root.join("logs/errors"))
        .ok()
        .map(|d| {
            d.map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(rows.len(), 4, "errors: {errs:?}");
    let (plain, first, second, modified) = (&rows[0], &rows[1], &rows[2], &rows[3]);
    // Same shim content in different temp dirs produces the exact same key,
    // differs from no preload and modified content, and stays pure.
    assert_ne!(plain["key"], first["key"]);
    assert_eq!(first["key"], second["key"]);
    assert_ne!(first["key"], modified["key"]);
    for row in [first, second, modified] {
        assert_eq!(row["observer"]["class"], "pure", "{row}");
        assert!(row["observer"]["reason"].is_null(), "{row}");
        assert_eq!(
            row["observer"]["other_reads"],
            serde_json::json!([]),
            "{row}"
        );
    }

    // Stage-one cache modes still reject LD_PRELOAD cleanly and run the test.
    let mut gate = stage_command(root, &binary, test, "gate", "with-preload");
    gate.env("LD_PRELOAD", &so_a);
    let out = gate.output().unwrap();
    assert!(out.status.success(), "{}", details(&out));
    assert!(details(&out).contains("LD_PRELOAD"));
    assert_eq!(
        stage_records(root).pop().unwrap()["cache"]["action"],
        "bypassed"
    );
}
