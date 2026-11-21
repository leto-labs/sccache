#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::process::{self, Command};
    use std::time::Duration;

    #[test]
    fn pure() {
        assert_eq!(2 + 2, 4);
        println!("stdout marker");
        eprintln!("stderr marker");
    }

    #[test]
    fn files() {
        assert!(!std::path::Path::new("absent.txt").exists());
        assert_eq!(std::fs::read_to_string("fixture.txt").unwrap(), "fixture\n");
        assert!(
            std::fs::read_dir(".")
                .unwrap()
                .any(|entry| entry.unwrap().file_name() == "fixture.txt")
        );
    }

    #[test]
    fn network() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let _peer = listener.accept().unwrap();
    }

    #[test]
    fn exec() {
        let child = Command::new("/bin/cat")
            .arg("fixture.txt")
            .output()
            .unwrap();
        assert!(child.status.success());
        assert_eq!(child.stdout, b"fixture\n");
    }

    #[test]
    fn other() {
        std::fs::write("outside-tmp.txt", b"write").unwrap();
        std::fs::remove_file("outside-tmp.txt").unwrap();
    }

    #[test]
    fn killed() {
        unsafe extern "C" {
            fn raise(sig: i32) -> i32;
        }
        // SAFETY: SIGTERM (15) is a valid signal number on Linux.
        unsafe { raise(15) };
    }

    #[test]
    fn timeout() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        println!("timeout ready");
        std::io::stdout().flush().unwrap();
        let _ = child.wait();
    }

    #[test]
    #[ignore]
    fn stdin() {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap();
        assert_eq!(input, "stdin marker");
        print!("{input}");
    }

    #[test]
    #[ignore]
    fn relay() {
        println!("relay ready");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    #[ignore]
    fn explicit_exit() {
        println!("exit stdout");
        eprintln!("exit stderr");
        process::exit(37);
    }
}
