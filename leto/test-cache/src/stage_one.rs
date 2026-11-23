use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::observer::Rules;
use crate::process::{self, Capture};
use crate::results::{Entry, OUTPUT_LIMIT, Store, Streams, test_identity};
use crate::{RunRecord, checkout, key, sandbox, store};

const VERIFICATION_FAILED: i32 = 125 << 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Replay,
    Gate,
}

impl Mode {
    pub fn selected() -> Option<Self> {
        match env::var("LETO_TEST_CACHE").as_deref() {
            Ok("replay") => Some(Self::Replay),
            Ok("gate") => Some(Self::Gate),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Replay => "replay",
            Self::Gate => "gate",
        }
    }
}

pub fn run(
    command: &mut Option<Command>,
    binary: &Path,
    args: &[OsString],
    test: &OsStr,
    mode: Mode,
    start: Instant,
) -> io::Result<i32> {
    let root = require_gate_log(command, mode, store::root_for("v1"))?;
    let now = store::clock();
    let checkout = checkout()?;
    let project = env::var_os("LETO_TEST_CACHE_PROJECT")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| checkout.file_name().unwrap_or_default().to_owned());
    let binary_id = env::var_os("NEXTEST_BINARY_ID").unwrap_or_default();
    let run_id = env::var_os("NEXTEST_RUN_ID")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("runner:{}", std::process::id()).into());
    let identity = test_identity(&project, &binary_id, test);
    let mut cache = Run {
        mode,
        action: "bypassed",
        observer: json!("unavailable"),
        verification: None,
        quarantined: false,
        binary,
        args,
        test,
        checkout: &checkout,
        run_id: &run_id,
    };
    let file = require_gate_log(
        command,
        mode,
        store::log_file(&root, "runs", &store::day(now.as_secs())),
    )?;
    let eligible = first_attempt()
        && !env::var_os("LLVM_PROFILE_FILE").is_some_and(|value| !value.is_empty())
        && binary.as_os_str().as_bytes().contains(&b'/');
    // Log a real outcome even when the key cannot be built or this attempt is ineligible.
    let keys = if eligible {
        match key::stage_one(&root, binary, args, &checkout, now, mode == Mode::Gate) {
            Ok(keys) => Some(keys),
            Err(error) => {
                cache.diagnostic(&error);
                None
            }
        }
    } else {
        None
    };
    let results = Store {
        root: &root,
        key: keys
            .as_ref()
            .map_or_else(|| blake3::hash(b"unkeyed"), |(id, _)| id.key),
        identity,
    };
    let child_start = Instant::now();
    let verification_failed = mode == Mode::Gate
        && require_gate_log(command, mode, results.failed_verification(&run_id))?;
    let status = if verification_failed {
        cache.action = "quarantined";
        cache.quarantined = true;
        cache.verification_error("a previous attempt in this run failed verification");
        VERIFICATION_FAILED
    } else if let Some((id, stage)) = &keys {
        match cache.run(command, &results, id, stage) {
            Err(error) if command.is_some() => {
                cache.action = "unavailable";
                cache.diagnostic(&error);
                plain(command)?
            }
            result => result?,
        }
    } else {
        plain(command)?
    };
    let record = RunRecord {
        file,
        now,
        binary,
        test,
        checkout: &checkout,
    };
    let cache_event = json!({
        "mode": mode.name(), "action": cache.action, "verification": cache.verification,
        "quarantined": cache.quarantined, "keyed": keys.is_some(),
    });
    let (digest, mtime) = keys.as_ref().map_or_else(
        || (blake3::hash(b"unkeyed"), (0, 0)),
        |(id, _)| (id.binary_digest, id.mtime),
    );
    record.write(
        results.key,
        digest,
        mtime,
        status,
        child_start.duration_since(start),
        child_start.elapsed(),
        1,
        cache.observer,
        Some(cache_event),
    )?;
    Ok(status)
}

fn require_gate_log<T>(
    command: &mut Option<Command>,
    mode: Mode,
    value: io::Result<T>,
) -> io::Result<T> {
    value.inspect_err(|_| {
        if mode == Mode::Gate {
            command.take();
        }
    })
}

fn first_attempt() -> bool {
    env::var("NEXTEST_ATTEMPT").as_deref() == Ok("1")
        && matches!(
            env::var("NEXTEST_TOTAL_ATTEMPTS").as_deref(),
            Err(env::VarError::NotPresent) | Ok("1")
        )
        && matches!(
            env::var("NEXTEST_STRESS_CURRENT").as_deref(),
            Err(_) | Ok("" | "none")
        )
}

struct Run<'a> {
    mode: Mode,
    action: &'static str,
    observer: Value,
    verification: Option<&'static str>,
    quarantined: bool,
    binary: &'a Path,
    args: &'a [OsString],
    test: &'a OsStr,
    checkout: &'a Path,
    run_id: &'a OsStr,
}

impl Run<'_> {
    fn diagnostic(&self, reason: &io::Error) {
        eprintln!(
            "leto test cache {} for {:?}: {reason}",
            self.mode.name(),
            self.test
        );
    }

    fn verification_error(&self, reason: &str) {
        eprintln!(
            "leto test cache verification failed for {:?}: {reason}",
            self.test
        );
    }

    fn run(
        &mut self,
        command: &mut Option<Command>,
        store: &Store<'_>,
        id: &key::Identity,
        stage: &key::StageOne,
    ) -> io::Result<i32> {
        if store.quarantined()? {
            self.action = "quarantined";
            self.quarantined = true;
            return plain(command);
        }
        match store.lookup(&stage.output_key)? {
            Some(Entry::Impure) => {
                self.action = "impure";
                return plain(command);
            }
            Some(Entry::Pass(streams)) if stage.unchanged() && !store.quarantined()? => {
                if self.mode == Mode::Gate && store.verify_hit(self.run_id)? {
                    return self.verify(command, store, &streams);
                }
                command.take();
                emit(&streams)?;
                self.action = "replayed";
                return Ok(0);
            }
            _ => {}
        }

        self.action = "miss";
        if self.mode == Mode::Replay {
            return plain(command);
        }
        let Some((capture, status)) = self.sandbox(command, store.root, id, stage)? else {
            self.action = "unavailable";
            return plain(command);
        };
        if let Some(signal) = cancelled(status) {
            return Ok(signal);
        }

        let clean =
            status == 0 && self.observer["class"] == "pure" && self.observer["denied"] == false;
        if clean {
            let duration = self.sandbox_duration();
            if stage.unchanged()
                && let Some(streams) = capture.output(OUTPUT_LIMIT)?
                && self.save(store, &Entry::Pass(streams), id, stage, duration)
            {
                self.action = "stored";
            }
            capture.emit()?;
            return Ok(status);
        }

        self.action = "impure";
        let duration = self.sandbox_duration();
        *command = Some(self.command());
        let actual = plain(command)?;
        if actual == 0
            && process::interrupted().is_none()
            && stage.unchanged()
            && self.observer["started"] == true
        {
            self.save(store, &Entry::Impure, id, stage, duration);
        }
        Ok(actual)
    }

    fn sandbox(
        &mut self,
        command: &mut Option<Command>,
        root: &Path,
        id: &key::Identity,
        stage: &key::StageOne,
    ) -> io::Result<Option<(Capture, i32)>> {
        let setup = || {
            sandbox::check_abi()?;
            Ok::<_, io::Error>((sandbox::Temp::new(root)?, Capture::new()?))
        };
        let (temp, capture) = match setup() {
            Ok(files) => files,
            Err(error) => {
                self.diagnostic(&error);
                self.observer = json!({"reason": error.to_string()});
                return Ok(None);
            }
        };
        let launch = sandbox::Launch::new(
            self.binary,
            self.args,
            stage.environment.clone(),
            &temp.0,
            &capture,
        )?;
        let rules = Rules {
            checkout: self.checkout.to_owned(),
            temp: temp.0.clone(),
            binary: self.binary.to_owned(),
            libraries: id.libraries.clone(),
        };
        let start = Instant::now();
        let Some((status, evidence)) = process::run_sandbox(command, rules, &launch)? else {
            let reason = "could not apply Landlock, seccomp or the observer";
            self.diagnostic(&io::Error::other(reason));
            self.observer = json!({"reason": reason});
            return Ok(None);
        };
        // Observation paths may themselves contain the value of an environment variable.
        self.observer = json!({
            "class": evidence["class"], "denied": evidence["denied"],
            "reason": evidence["reason"], "started": evidence["started"],
            "duration_ns": start.elapsed().as_nanos() as u64,
        });
        Ok(Some((capture, status)))
    }

    fn sandbox_duration(&self) -> Duration {
        Duration::from_nanos(self.observer["duration_ns"].as_u64().unwrap_or_default())
    }

    fn save(
        &self,
        store: &Store<'_>,
        entry: &Entry,
        id: &key::Identity,
        stage: &key::StageOne,
        duration: Duration,
    ) -> bool {
        match store.save(entry, &stage.output_key, id.binary_digest, duration) {
            Ok(saved) => saved,
            Err(error) => {
                self.diagnostic(&error);
                false
            }
        }
    }

    fn verify(
        &mut self,
        command: &mut Option<Command>,
        store: &Store<'_>,
        expected: &Streams,
    ) -> io::Result<i32> {
        let capture = Capture::new()?;
        capture.command(command.as_mut().unwrap())?;
        let status = plain(command)?;
        if let Some(signal) = cancelled(status) {
            self.verification = Some("cancelled");
            capture.emit()?;
            return Ok(signal);
        }
        let matches = status == 0
            && capture
                .output(OUTPUT_LIMIT)?
                .is_some_and(|(stdout, stderr)| {
                    same_stdout(&stdout, &expected.0) && stderr == expected.1
                });
        if matches {
            capture.emit()?;
            self.action = "verified";
            self.verification = Some("matched");
            return Ok(0);
        }

        self.action = "quarantined";
        self.verification = Some("mismatch");
        self.quarantined = true;
        match store.quarantine(self.run_id) {
            Ok(()) => {
                self.verification_error("uncached exit status or output changed; test quarantined")
            }
            Err(error) => {
                self.verification_error("uncached exit status or output changed");
                self.diagnostic(&error);
                self.quarantined = false;
            }
        }
        if let Err(error) = capture.emit() {
            self.diagnostic(&error);
        }
        Ok(VERIFICATION_FAILED)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.binary);
        command.args(self.args);
        command
    }
}

fn plain(command: &mut Option<Command>) -> io::Result<i32> {
    process::run(command, None).map(|(status, _)| process::interrupted().unwrap_or(status))
}

fn cancelled(status: i32) -> Option<i32> {
    process::interrupted().or_else(|| {
        (libc::WIFSIGNALED(status)
            && matches!(
                libc::WTERMSIG(status),
                libc::SIGINT | libc::SIGTERM | libc::SIGHUP
            ))
        .then_some(status)
    })
}

fn emit((stdout, stderr): &Streams) -> io::Result<()> {
    io::stdout().lock().write_all(stdout)?;
    io::stderr().lock().write_all(stderr)
}

fn same_stdout(actual: &[u8], expected: &[u8]) -> bool {
    // Libtest includes the measured wall time in its final summary line.
    fn without_libtest_time(stdout: &[u8]) -> Option<(&[u8], &[u8])> {
        if !stdout.starts_with(b"\nrunning 1 test\n") {
            return None;
        }
        let prefix = b"\ntest result: ok. ";
        let summary = stdout
            .windows(prefix.len())
            .rposition(|window| window == prefix)?;
        let mut start = summary + prefix.len();
        for label in [
            b" passed; ".as_slice(),
            b" failed; ",
            b" ignored; ",
            b" measured; ",
            b" filtered out; finished in ",
        ] {
            let digits = stdout[start..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits == 0 || !stdout[start + digits..].starts_with(label) {
                return None;
            }
            start += digits + label.len();
        }
        let end = stdout[start..].iter().position(|&byte| byte == b's')? + start;
        let time = &stdout[start..end];
        let (seconds, hundredths) = time.split_at(time.iter().position(|&byte| byte == b'.')?);
        if seconds.is_empty()
            || !seconds.iter().all(u8::is_ascii_digit)
            || hundredths.len() != 3
            || !hundredths[1..].iter().all(u8::is_ascii_digit)
            || !matches!(&stdout[end..], b"s\n" | b"s\n\n")
        {
            return None;
        }
        Some((&stdout[..start], &stdout[end..]))
    }
    actual == expected
        || without_libtest_time(actual)
            .zip(without_libtest_time(expected))
            .is_some_and(|(actual, expected)| actual == expected)
}
