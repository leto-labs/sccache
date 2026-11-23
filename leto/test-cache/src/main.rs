mod key;
mod observer;
mod process;
mod results;
mod sandbox;
mod stage_one;
mod store;

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;

const DEFAULT_SAMPLE: u64 = 5;

fn main() {
    let mut cli = env::args_os().skip(1);
    let Some(binary) = cli.next() else {
        eprintln!("usage: leto-test-runner <test-binary> [args...]");
        std::process::exit(2);
    };
    let args: Vec<_> = cli.collect();
    let mut command = Some(Command::new(&binary));
    command.as_mut().unwrap().args(&args);
    let Some(test) = test_name(&args) else {
        process::exec(command.take().unwrap())
    };
    let binary = Path::new(&binary);
    let start = Instant::now();
    if let Some(mode) = stage_one::Mode::selected() {
        let status = stage_one::run(&mut command, binary, &args, &test, mode, start)
            .unwrap_or_else(|error| {
                eprintln!("leto test cache: {error}");
                if let Some(command) = command.take() {
                    process::exec(command);
                }
                125 << 8
            });
        process::finish(status);
    }
    let root = match store::root() {
        Ok(root) => root,
        Err(error) => {
            store::error(None, binary, &error);
            process::exec(command.take().unwrap());
        }
    };
    match shadow(&mut command, &root, binary, &args, &test, start) {
        Ok(status) => process::finish(status),
        Err(error) => {
            store::error(Some(&root), binary, &error);
            if let Some(command) = command {
                process::exec(command);
            }
            // A test must never be launched again after the first fork.
            std::process::exit(125);
        }
    }
}

fn test_name(args: &[OsString]) -> Option<OsString> {
    if env::var_os("LETO_TEST_CACHE").as_deref() == Some(OsStr::new("off"))
        || args.iter().any(|arg| arg == "--list")
        || env::var_os("NEXTEST_BINARY_ID").is_none()
    {
        return None;
    }
    let name = env::var_os("NEXTEST_TEST_NAME")?;
    // Nextest identifies a per-test harness invocation with the exact filter.
    args.iter().any(|arg| arg == "--exact").then_some(())?;
    args.contains(&name).then_some(name)
}

fn checkout() -> io::Result<PathBuf> {
    if let Some(root) =
        env::var_os("NEXTEST_WORKSPACE_ROOT").or_else(|| env::var_os("LETO_TEST_CACHE_WORKSPACE"))
    {
        return Ok(PathBuf::from(root));
    }
    env::current_dir()
}

fn sample(key: blake3::Hash, now: Duration, every: u64) -> bool {
    let phase = u64::from_le_bytes(key.as_bytes()[..8].try_into().unwrap());
    phase % every == (now.as_secs() / 86_400) % every
}

fn sample_rate() -> io::Result<u64> {
    match env::var("LETO_TEST_CACHE_OBSERVE") {
        Err(env::VarError::NotPresent) => Ok(DEFAULT_SAMPLE),
        Ok(value) => {
            value.parse().ok().filter(|&n: &u64| n != 0).ok_or_else(|| {
                io::Error::other("LETO_TEST_CACHE_OBSERVE must be a positive integer")
            })
        }
        Err(error) => Err(io::Error::other(error)),
    }
}

fn shadow(
    command: &mut Option<Command>,
    root: &Path,
    binary: &Path,
    args: &[OsString],
    test: &OsStr,
    start: Instant,
) -> io::Result<i32> {
    let now = store::clock();
    let checkout = checkout()?;
    let identity = key::build(root, binary, args, &checkout, now)?;
    let every = sample_rate()?;
    let file = store::log_file(root, "runs", &store::day(now.as_secs()))?;
    let rules = sample(identity.key, now, every).then(|| observer::Rules {
        checkout: checkout.clone(),
        temp: env::temp_dir(),
        binary: binary.to_owned(),
        libraries: identity.libraries,
    });
    let child_start = Instant::now();
    let (status, evidence) = process::run(command, rules)?;
    let duration = child_start.elapsed();
    let context = RunRecord {
        file,
        now,
        binary,
        test,
        checkout: &checkout,
    };
    if let Err(error) = context.write(
        identity.key,
        identity.binary_digest,
        identity.mtime,
        status,
        child_start.duration_since(start),
        duration,
        every,
        evidence,
        None,
    ) {
        store::error(Some(root), binary, &error);
    }
    Ok(status)
}

struct RunRecord<'a> {
    file: File,
    now: Duration,
    binary: &'a Path,
    test: &'a OsStr,
    checkout: &'a Path,
}

impl RunRecord<'_> {
    #[allow(clippy::too_many_arguments)]
    fn write(
        self,
        key: blake3::Hash,
        digest: blake3::Hash,
        mtime: (i64, i64),
        status: i32,
        setup: Duration,
        duration: Duration,
        rate: u64,
        evidence: serde_json::Value,
        cache: Option<serde_json::Value>,
    ) -> io::Result<()> {
        let env = |key| env::var(key).unwrap_or_default();
        let project = env("LETO_TEST_CACHE_PROJECT");
        let project = if project.is_empty() {
            self.checkout
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            project
        };
        let mut record = json!({
            "key": key.to_hex().as_str(), "digest": digest.to_hex().as_str(), "mtime": mtime,
            "binary": self.binary.to_string_lossy(), "test": self.test.to_string_lossy(),
            "worktree": self.checkout.to_string_lossy(), "project": project,
            "binary_id": env("NEXTEST_BINARY_ID"), "profile": env("NEXTEST_PROFILE"),
            "run_id": env("NEXTEST_RUN_ID"),
            "attempt": env("NEXTEST_ATTEMPT").parse::<u32>().ok(),
            "stress": !matches!(env("NEXTEST_STRESS_CURRENT").as_str(), "" | "none"),
            "time_ns": self.now.as_nanos() as u64, "duration_ns": duration.as_nanos() as u64,
            "setup_ns": setup.as_nanos() as u64, "observe_every": rate,
            "outcome": process::outcome(status), "observer": evidence,
        });
        if let Some(cache) = cache {
            record["cache"] = cache;
        }
        for limit in [None, Some(180), Some(32)] {
            if let Some(limit) = limit {
                for field in [
                    "binary",
                    "test",
                    "worktree",
                    "project",
                    "binary_id",
                    "profile",
                    "run_id",
                ] {
                    if let Some(value) = record[field].as_str() {
                        record[field] = json!(store::clipped(value, limit));
                    }
                }
                record["identity_truncated"] = json!(true);
            }
            match store::append(&self.file, &record) {
                Err(error) if error.kind() == io::ErrorKind::InvalidData && limit != Some(32) => {}
                result => return result,
            }
        }
        unreachable!("final log append returns its result")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_rotates_through_one_day_per_period() {
        let k = blake3::hash(b"sample");
        assert!(sample(k, Duration::ZERO, 1));
        let days = (0..14)
            .filter(|day| sample(k, Duration::from_secs(day * 86_400), DEFAULT_SAMPLE))
            .collect::<Vec<_>>();
        assert!(days.len() >= (14 / DEFAULT_SAMPLE) as usize);
        assert!(
            days.windows(2)
                .all(|window| window[1] - window[0] == DEFAULT_SAMPLE)
        );
    }
}
