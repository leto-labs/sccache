use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_leto-jobserver"))
}

struct TestDaemon {
    child: Child,
    work: PathBuf,
    link_path: PathBuf,
    socket_path: PathBuf,
    psi_dir: PathBuf,
}

impl TestDaemon {
    fn start(tokens: usize, max_per_owner: usize, psi_enabled: bool) -> Self {
        let rt = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
        let unique = format!(
            "leto-js-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let work = rt.join(unique);
        let mount_dir = work.join("mnt");
        let link_path = work.join("build-jobserver");
        let socket_path = work.join("ctl.sock");
        let psi_dir = work.join("psi");
        fs::create_dir_all(&psi_dir).unwrap();
        fs::write(
            psi_dir.join("io"),
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();
        fs::write(
            psi_dir.join("memory"),
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();

        let mut cmd = Command::new(bin());
        cmd.arg("serve")
            .arg("--tokens")
            .arg(tokens.to_string())
            .arg("--mount-dir")
            .arg(&mount_dir)
            .arg("--path")
            .arg(&link_path)
            .arg("--socket")
            .arg(&socket_path)
            .arg("--max-per-owner")
            .arg(max_per_owner.to_string());
        if psi_enabled {
            cmd.arg("--psi-dir")
                .arg(&psi_dir)
                .arg("--psi-interval-ms")
                .arg("50")
                .arg("--psi-window")
                .arg("3")
                .arg("--psi-high")
                .arg("20.0")
                .arg("--psi-low")
                .arg("8.0")
                .arg("--psi-step")
                .arg("2")
                .arg("--psi-max-withhold-fraction")
                .arg("0.5");
        } else {
            cmd.arg("--psi-disabled");
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn leto-jobserver");

        let d = Self {
            child,
            work,
            link_path,
            socket_path,
            psi_dir,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if d.socket_path.exists() && d.link_path.exists() && d.status().is_ok() {
                return d;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("leto-jobserver did not become ready");
    }

    fn ctl(&self, req: Value) -> Result<Value, String> {
        let mut s = UnixStream::connect(&self.socket_path).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        s.write_all(&line).map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(s);
        let mut reply = String::new();
        reader.read_line(&mut reply).map_err(|e| e.to_string())?;
        serde_json::from_str(&reply).map_err(|e| e.to_string())
    }

    fn status(&self) -> Result<Value, String> {
        self.ctl(json!({ "cmd": "status" }))
    }

    fn wait_status<F: Fn(&Value) -> bool>(&self, desc: &str, pred: F) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = json!(null);
        while Instant::now() < deadline {
            if let Ok(st) = self.status() {
                if pred(&st) {
                    return st;
                }
                last = st;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        panic!("timeout waiting for {desc}; last={last}");
    }

    fn set_psi_io(&self, full_avg10: f64) {
        let content = format!(
            "some avg10={full_avg10:.2} avg60=0.00 avg300=0.00 total=0\nfull avg10={full_avg10:.2} avg60=0.00 avg300=0.00 total=0\n"
        );
        let tmp = self.psi_dir.join("io.tmp");
        fs::write(&tmp, content).unwrap();
        fs::rename(&tmp, self.psi_dir.join("io")).unwrap();
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.work);
    }
}

fn open_rw(path: &Path, nonblock: bool) -> fs::File {
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true);
    if nonblock {
        opts.custom_flags(libc::O_NONBLOCK);
    }
    opts.open(path).expect("open jobserver token file")
}

#[test]
fn nonblock_poll_resize_and_cap_over_fuse() {
    let d = TestDaemon::start(4, 2, false);
    assert_eq!(d.status().unwrap()["available"], 4);

    // Dual-open like jobserver-rs; cap=2 limits this process to 2 tokens even with 4 available.
    let mut r = open_rw(&d.link_path, true);
    let mut w = open_rw(&d.link_path, false);
    let mut b = [0u8; 1];
    assert_eq!(r.read(&mut b).unwrap(), 1);
    assert_eq!(r.read(&mut b).unwrap(), 1);
    assert_eq!(
        r.read(&mut b).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    // poll(POLLIN) must NOT report readable while capped, so capped clients never spin.
    let mut pfd = libc::pollfd {
        fd: r.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
    assert_eq!(rc, 0, "capped owner must not wake POLLIN");

    // Returning 1 token via separate write fh wakes the same TGID.
    assert_eq!(w.write(b"+").unwrap(), 1);
    let rc = unsafe { libc::poll(&mut pfd, 1, 1000) };
    assert_eq!(rc, 1);
    assert_ne!(pfd.revents & libc::POLLIN, 0);
    assert_eq!(r.read(&mut b).unwrap(), 1);

    // Live resize up + raising per-owner cap allows taking 2 more tokens.
    let st = d.ctl(json!({ "cmd": "resize", "capacity": 6 })).unwrap();
    assert_eq!(st["capacity"], 6);
    let st = d
        .ctl(json!({ "cmd": "set_max_per_owner", "max_per_owner": 0 }))
        .unwrap();
    assert_eq!(st["max_per_owner"], 0);
    assert_eq!(r.read(&mut b).unwrap(), 1);
    assert_eq!(r.read(&mut b).unwrap(), 1);
    assert_eq!(d.status().unwrap()["held"], 4);

    drop(r);
    drop(w);
    d.wait_status("reclaim after drop", |s| {
        s["held"] == 0 && s["available"] == 6 && s["reclaimed_total"] == 4
    });
}

#[test]
fn sigkill_reclaims_tokens_immediately() {
    let d = TestDaemon::start(4, 0, false);
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import os, time\nr=os.open({p:?}, os.O_RDWR)\nw=os.open({p:?}, os.O_RDWR)\nfor _ in range(3): assert os.read(r, 1)==b'+'\nprint('READY', flush=True)\ntime.sleep(60)\n",
            p = d.link_path.to_str().unwrap()
        ))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "READY");
    assert_eq!(d.status().unwrap()["held"], 3);

    child.kill().unwrap();
    child.wait().unwrap();
    let st = d.wait_status("SIGKILL reclaim", |s| {
        s["held"] == 0 && s["available"] == 4 && s["reclaimed_total"] == 3
    });
    assert_eq!(st["owners"], 0);
}

#[test]
fn psi_withholds_and_releases_tokens_live() {
    let d = TestDaemon::start(6, 0, true);
    // Drive io full avg10 above high=20 -> cap = floor(6*0.5) = 3 withheld -> effective = 3.
    d.set_psi_io(40.0);
    let st = d.wait_status("PSI withhold to cap 3", |s| {
        s["withheld"] == 3 && s["effective_capacity"] == 3
    });
    assert_eq!(st["available"], 3);

    // Drop io full avg10 below low=8 -> releases back to withheld=0, effective=6.
    d.set_psi_io(1.0);
    let st = d.wait_status("PSI release to 0", |s| {
        s["withheld"] == 0 && s["effective_capacity"] == 6
    });
    assert_eq!(st["available"], 6);
}

#[test]
fn gnu_make_throttles_and_sigkill_reclaims() {
    let d = TestDaemon::start(2, 0, false);
    let mk = d.work.join("mk");
    fs::create_dir_all(&mk).unwrap();
    fs::write(
        mk.join("Makefile"),
        "all: t1 t2 t3 t4 t5 t6\nt1 t2 t3 t4 t5 t6:\n\t@sleep 0.25\n",
    )
    .unwrap();
    let t0 = Instant::now();
    let status = Command::new("make")
        .arg("-C")
        .arg(&mk)
        .env(
            "MAKEFLAGS",
            format!("-j --jobserver-auth=fifo:{}", d.link_path.display()),
        )
        .env_remove("CARGO_MAKEFLAGS")
        .status()
        .unwrap();
    assert!(status.success());
    // 6 tasks * 0.25s with 1 implicit + 2 FUSE tokens = 3 concurrent => ~0.50s
    assert!(
        t0.elapsed() >= Duration::from_millis(450),
        "make did not throttle: {:?}",
        t0.elapsed()
    );
    d.wait_status("make clean", |s| s["held"] == 0 && s["available"] == 2);
}

#[test]
fn rustc_parallel_codegen_sigkill_reclaims_tokens() {
    let d = TestDaemon::start(4, 0, false);
    let rs = d.work.join("heavy.rs");
    let mut src = String::new();
    for m in 0..16 {
        src.push_str(&format!("pub mod m{m} {{\n"));
        for i in 0..120 {
            src.push_str(&format!(
                "#[inline(never)] pub fn f{i}(mut x: u64) -> u64 {{ for k in 0..300u64 {{ x = x.wrapping_mul(k.wrapping_add({})).rotate_left(((k+x)&63) as u32) ^ (x >> 13); }} x }}\n",
                m * 1000 + i + 1
            ));
        }
        src.push_str("}\n");
    }
    fs::write(&rs, src).unwrap();

    let mut child = Command::new("rustc")
        .args([
            "--crate-type=lib",
            "-O",
            "-C",
            "codegen-units=16",
            rs.to_str().unwrap(),
            "-o",
            d.work.join("libheavy.rlib").to_str().unwrap(),
        ])
        .env(
            "CARGO_MAKEFLAGS",
            format!("-j --jobserver-auth=fifo:{}", d.link_path.display()),
        )
        .env_remove("MAKEFLAGS")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    d.wait_status("rustc holding >= 2 tokens", |s| {
        s["held"].as_u64().unwrap_or(0) >= 2
    });
    child.kill().unwrap();
    child.wait().unwrap();
    let st = d.wait_status("rustc SIGKILL reclaim", |s| {
        s["held"] == 0 && s["available"] == 4 && s["reclaimed_total"].as_u64().unwrap_or(0) >= 2
    });
    assert_eq!(st["owners"], 0);
}
