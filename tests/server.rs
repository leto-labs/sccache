#[cfg(not(target_os = "macos"))]
use std::{net::TcpListener, process::Command};

#[cfg(not(target_os = "macos"))]
use serial_test::serial;

#[test]
#[serial]
// test fails intermittently on macos:
// https://github.com/mozilla/sccache/issues/234
#[cfg(not(target_os = "macos"))]
fn test_server_port_in_use() {
    // Bind an arbitrary free port.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let sccache = env!("CARGO_BIN_EXE_sccache");
    let output = Command::new(sccache)
        .arg("--start-server")
        .env(
            "SCCACHE_SERVER_PORT",
            listener.local_addr().unwrap().port().to_string(),
        )
        .env_remove("SCCACHE_SERVER_UDS")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let s = String::from_utf8_lossy(&output.stderr);
    const MSG: &str = "Server startup failed:";
    assert!(
        s.contains(MSG),
        "Output did not contain '{}':\n========\n{}\n========",
        MSG,
        s
    );
}

#[cfg(unix)]
#[allow(dead_code, unused_imports)]
mod harness;

#[cfg(unix)]
mod restart {
    use super::harness::sccache_command;
    use assert_cmd::prelude::*;
    use predicates::prelude::*;
    use serde_json::Value;
    use std::fs;
    use std::io;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Child, Command, Output, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use test_case::test_case;

    struct Process(Option<Child>);

    impl Process {
        fn spawn(cmd: &mut Command) -> Self {
            Self(Some(
                cmd.stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            ))
        }

        fn child(&mut self) -> &mut Child {
            self.0.as_mut().unwrap()
        }

        fn signal(&mut self, signal: libc::c_int) {
            assert!(self.child().try_wait().unwrap().is_none());
            let pid = self.child().id().try_into().unwrap();
            // The child still belongs to this fixture and has not been reaped.
            assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
        }

        fn wait(mut self) -> Output {
            wait_for(|| self.child().try_wait().unwrap().is_some());
            self.0.take().unwrap().wait_with_output().unwrap()
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn wait_for(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for test process"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    struct Fixture {
        dir: TempDir,
        addr: SocketAddr,
    }

    impl Fixture {
        fn new(drain: u64, autostart: bool, wait: u64) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let addr = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let log = dir.path().join("explain/misses.jsonl");
            fs::write(
                dir.path().join("config"),
                format!(
                    "miss_explain = {}\n[server]\nshutdown_drain_secs = {drain}\nautostart = {autostart}\nconnect_wait_secs = {wait}\n",
                    toml::Value::String(log.to_str().unwrap().into())
                ),
            )
            .unwrap();
            fs::write(dir.path().join("lib.rs"), "pub fn test() {}\n").unwrap();
            let compiler = dir.path().join("rustc");
            fs::write(
                &compiler,
                r#"#!/bin/sh
case "${1-}" in +*) exit 1 ;; esac
compile=false
for arg do
    case "$arg" in
        --print*) exec rustc "$@" ;;
        link|--emit=link) compile=true ;;
    esac
done
if "$compile"; then
    first=false
    if [ ! -e compiler-started ]; then first=true; fi
    printf '%s\n' "$$" >> compiler-started
    if "$first"; then sleep "${SCCACHE_TEST_COMPILE_DELAY:-0}"; fi
    if [ -n "$SCCACHE_TEST_COMPILER_EXIT" ]; then exit "$SCCACHE_TEST_COMPILER_EXIT"; fi
fi
exec rustc "$@"
"#,
            )
            .unwrap();
            fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir, addr }
        }

        fn command(&self) -> Command {
            let mut cmd = sccache_command();
            cmd.current_dir(self.dir.path())
                .env("SCCACHE_DIR", self.dir.path().join("cache"))
                .env("SCCACHE_CONF", self.dir.path().join("config"))
                .env("SCCACHE_SERVER_PORT", self.addr.port().to_string())
                .env("SCCACHE_IDLE_TIMEOUT", "0");
            cmd
        }

        fn compile(&self, sleep: &str) -> Command {
            let mut cmd = self.command();
            cmd.arg(self.dir.path().join("rustc"))
                .args([
                    "--crate-name",
                    "slow",
                    "lib.rs",
                    "--crate-type",
                    "lib",
                    "--emit=link",
                    "--out-dir",
                    ".",
                ])
                .env("SCCACHE_TEST_COMPILE_DELAY", sleep);
            cmd
        }

        fn start_server(&self) -> Process {
            let mut server = Process::spawn(
                self.command()
                    .env("SCCACHE_START_SERVER", "1")
                    .env("SCCACHE_NO_DAEMON", "1"),
            );
            wait_for(|| {
                if server.child().try_wait().unwrap().is_some() {
                    panic!("server exited before it accepted connections");
                }
                TcpStream::connect(self.addr).is_ok()
            });
            server
        }

        fn stop_server(&self) {
            self.command().arg("--stop-server").assert().success();
        }

        fn wait_for_compile(&self) {
            wait_for(|| self.dir.path().join("compiler-started").exists());
        }

        fn wait_for_closed_listener(&self) {
            wait_for(|| {
                matches!(
                    TcpStream::connect(self.addr),
                    Err(err) if err.kind() == io::ErrorKind::ConnectionRefused
                )
            });
        }

        fn assert_refused(&self) {
            assert_eq!(
                TcpStream::connect(self.addr).unwrap_err().kind(),
                io::ErrorKind::ConnectionRefused
            );
        }

        fn assert_compiler_invocations(&self, count: usize) {
            let recorded = fs::read_to_string(self.dir.path().join("compiler-started")).unwrap();
            assert_eq!(recorded.lines().count(), count, "{recorded}");
        }

        fn stats(&self) -> Value {
            serde_json::from_slice(
                &self
                    .command()
                    .args(["--show-stats", "--stats-format=json"])
                    .assert()
                    .success()
                    .get_output()
                    .stdout,
            )
            .unwrap()
        }

        fn path(&self, path: &str) -> std::path::PathBuf {
            self.dir.path().join(Path::new(path))
        }
    }

    #[test_case(None ; "stop RPC")]
    #[test_case(Some(libc::SIGTERM) ; "SIGTERM")]
    #[test_case(Some(libc::SIGINT) ; "SIGINT")]
    fn test_server_drains_compile_and_writes_cache(signal: Option<libc::c_int>) {
        let fixture = Fixture::new(5, false, 0);
        let mut server = fixture.start_server();
        let mut compile = Process::spawn(&mut fixture.compile("2"));
        fixture.wait_for_compile();

        if let Some(signal) = signal {
            server.signal(signal);
        } else {
            fixture.stop_server();
        }
        fixture.wait_for_closed_listener();
        assert!(server.child().try_wait().unwrap().is_none());
        assert!(compile.child().try_wait().unwrap().is_none());
        fixture.assert_refused();
        let second = fixture.path("second");
        fs::create_dir(&second).unwrap();
        fs::copy(fixture.path("lib.rs"), second.join("lib.rs")).unwrap();
        fixture
            .compile("0")
            .current_dir(&second)
            .assert()
            .success()
            .stderr(predicate::str::contains("compiling locally"));
        assert!(second.join("libslow.rlib").exists());

        compile
            .wait()
            .assert()
            .success()
            .stderr(predicate::str::is_empty());
        server.wait().assert().success();
        let log = fs::read_to_string(fixture.path("explain/misses.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 1);
        fs::remove_file(fixture.path("libslow.rlib")).unwrap();

        let replacement = fixture.start_server();
        fixture.compile("2").assert().success();
        fixture.assert_compiler_invocations(1);
        assert_eq!(fixture.stats()["stats"]["cache_hits"]["counts"]["Rust"], 1);
        assert!(fixture.path("libslow.rlib").exists());
        fixture.stop_server();
        replacement.wait().assert().success();
    }

    #[test]
    fn test_short_drain_resets_compile_and_client_falls_back() {
        let fixture = Fixture::new(1, false, 0);
        let server = fixture.start_server();
        let compile = Process::spawn(&mut fixture.compile("3"));
        fixture.wait_for_compile();

        fixture.stop_server();
        fixture.wait_for_closed_listener();
        server.wait().assert().success();
        compile
            .wait()
            .assert()
            .success()
            .stderr(predicate::str::contains(
                "The server looks like it shut down unexpectedly, compiling locally instead",
            ));
        fixture.assert_compiler_invocations(2);
    }

    #[test]
    fn test_second_sigterm_ends_drain() {
        let fixture = Fixture::new(30, false, 0);
        let mut server = fixture.start_server();
        let compile = Process::spawn(&mut fixture.compile("20"));
        fixture.wait_for_compile();

        server.signal(libc::SIGTERM);
        fixture.wait_for_closed_listener();
        let forced = Instant::now();
        server.signal(libc::SIGTERM);
        server.wait().assert().success();
        assert!(forced.elapsed() < Duration::from_secs(3));
        compile
            .wait()
            .assert()
            .success()
            .stderr(predicate::str::contains(
                "The server looks like it shut down unexpectedly, compiling locally instead",
            ));
        fixture.assert_compiler_invocations(2);
    }

    #[test_case("0", 0 ; "server side")]
    #[test_case("1", 23 ; "client side compiler exit status")]
    fn test_autostart_off_compiles_locally(client_side: &str, compiler_exit: i32) {
        let fixture = Fixture::new(5, true, 30);
        let start = Instant::now();
        fixture
            .compile("0")
            .env("SCCACHE_SERVER_AUTOSTART", "0")
            .env("SCCACHE_SERVER_CONNECT_WAIT_SECS", "1")
            .env("SCCACHE_CLIENT_SIDE", client_side)
            .env("SCCACHE_TEST_COMPILER_EXIT", compiler_exit.to_string())
            .assert()
            .code(compiler_exit)
            .stderr(predicate::str::contains("compiling locally").count(1));
        assert!(start.elapsed() >= Duration::from_secs(1));
        fixture.assert_compiler_invocations(1);
        fixture.assert_refused();
        TcpListener::bind(fixture.addr).unwrap();
    }

    #[test]
    fn test_autostart_off_waits_for_managed_server() {
        let fixture = Fixture::new(5, false, 5);
        fixture.assert_refused();
        let mut compile = Process::spawn(&mut fixture.compile("0"));
        thread::sleep(Duration::from_millis(200));
        assert!(compile.child().try_wait().unwrap().is_none());
        fixture.assert_refused();

        let server = fixture.start_server();
        compile
            .wait()
            .assert()
            .success()
            .stderr(predicate::str::is_empty());
        assert_eq!(fixture.stats()["stats"]["compile_requests"], 1);
        fixture.stop_server();
        server.wait().assert().success();
    }
}
