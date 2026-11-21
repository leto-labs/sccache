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
        let mut shadow = Command::new(RUNNER);
        configure(&mut shadow, root, "tests::pure");
        shadow.arg(&binary).args(args);
        // SAFETY: the hook passes initialized filter data to libc. It runs
        // only in the command subprocess before exec.
        unsafe {
            shadow.pre_exec(move || {
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
        command.args(args).output().unwrap()
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
        "5 passed",
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
        7,
        "{shadow_text}\n{records:#?}\n{:?}",
        fs::read_dir(root.join("logs/errors")).map(|entries| entries
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>())
    );
    for (test, class) in [
        ("pure", "pure"),
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
        match test {
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

    let mut checked = 0;
    for path in files_under(&root.join("logs"))
        .into_iter()
        .chain(files_under(&root.join("report")))
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
