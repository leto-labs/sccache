use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem::{self, MaybeUninit};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::state::{Effect, JobserverState};

const MAX_CLIENTS: usize = 16;
const MAX_REQUEST_BYTES: usize = 4096;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum ControlCommand {
    Status,
    Resize { capacity: usize },
    SetMaxPerOwner { max_per_owner: usize },
}

struct ClientConn {
    stream: UnixStream,
    deadline: Instant,
    in_buf: Vec<u8>,
    out_buf: Option<Vec<u8>>,
    out_pos: usize,
}

impl ClientConn {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            deadline: Instant::now() + CLIENT_TIMEOUT,
            in_buf: Vec::with_capacity(256),
            out_buf: None,
            out_pos: 0,
        }
    }

    fn raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    fn wants_write(&self) -> bool {
        self.out_buf.is_some()
    }
}

pub struct ControlServer {
    path: PathBuf,
    listener: UnixListener,
    clients: Vec<ClientConn>,
    uid: u32,
}

impl ControlServer {
    pub fn bind(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_file_name(format!(
            ".{}.tmp.{}",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("leto-jobserver.sock"),
            std::process::id()
        ));
        let _ = fs::remove_file(&tmp);
        let listener = UnixListener::bind(&tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, &path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            path,
            listener,
            clients: Vec::new(),
            uid: unsafe { libc::getuid() },
        })
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn client_pollfds(&self) -> impl Iterator<Item = (RawFd, i16)> + '_ {
        self.clients.iter().map(|c| {
            let events = if c.wants_write() {
                libc::POLLOUT
            } else {
                libc::POLLIN
            };
            (c.raw_fd(), events)
        })
    }

    pub fn accept_ready(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.clients.len() >= MAX_CLIENTS {
                        drop(stream);
                        continue;
                    }
                    if !peer_uid_matches(&stream, self.uid) {
                        drop(stream);
                        continue;
                    }
                    if stream.set_nonblocking(true).is_ok() {
                        self.clients.push(ClientConn::new(stream));
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    pub fn handle_client_fd(
        &mut self,
        fd: RawFd,
        revents: i16,
        state: &mut JobserverState,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(idx) = self.clients.iter().position(|c| c.raw_fd() == fd) else {
            return effects;
        };
        let mut drop_conn = (revents & (libc::POLLERR | libc::POLLNVAL)) != 0;
        if !drop_conn
            && !self.clients[idx].wants_write()
            && (revents & (libc::POLLIN | libc::POLLHUP)) != 0
        {
            let mut chunk = [0u8; 512];
            loop {
                match self.clients[idx].stream.read(&mut chunk) {
                    Ok(0) => {
                        if !self.clients[idx].in_buf.is_empty() {
                            let (resp, eff) = dispatch(&self.clients[idx].in_buf, state);
                            effects.extend(eff);
                            self.clients[idx].out_buf = Some(resp);
                        } else {
                            drop_conn = true;
                        }
                        break;
                    }
                    Ok(n) => {
                        self.clients[idx].in_buf.extend_from_slice(&chunk[..n]);
                        if self.clients[idx].in_buf.len() > MAX_REQUEST_BYTES {
                            let resp = error_line("request too large");
                            self.clients[idx].out_buf = Some(resp);
                            break;
                        }
                        if let Some(pos) = self.clients[idx].in_buf.iter().position(|&b| b == b'\n')
                        {
                            let line = self.clients[idx].in_buf[..pos].to_vec();
                            let (resp, eff) = dispatch(&line, state);
                            effects.extend(eff);
                            self.clients[idx].out_buf = Some(resp);
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        drop_conn = true;
                        break;
                    }
                }
            }
        }
        if !drop_conn && self.clients[idx].wants_write() {
            loop {
                let conn = &mut self.clients[idx];
                let buf = conn.out_buf.as_ref().unwrap();
                match conn.stream.write(&buf[conn.out_pos..]) {
                    Ok(0) => {
                        drop_conn = true;
                        break;
                    }
                    Ok(n) => {
                        conn.out_pos += n;
                        if conn.out_pos >= buf.len() {
                            drop_conn = true;
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        drop_conn = true;
                        break;
                    }
                }
            }
        }
        if drop_conn {
            self.clients.swap_remove(idx);
        }
        effects
    }

    pub fn expire_idle(&mut self) {
        let now = Instant::now();
        self.clients.retain(|c| c.deadline > now);
    }

    pub fn cleanup(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn peer_uid_matches(stream: &UnixStream, expected_uid: u32) -> bool {
    let mut cred: libc::ucred = unsafe { MaybeUninit::zeroed().assume_init() };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    rc == 0 && len as usize == mem::size_of::<libc::ucred>() && cred.uid == expected_uid
}

fn error_line(msg: &str) -> Vec<u8> {
    let mut out = serde_json::to_vec(&json!({ "ok": false, "error": msg })).unwrap_or_default();
    out.push(b'\n');
    out
}

fn dispatch(line: &[u8], state: &mut JobserverState) -> (Vec<u8>, Vec<Effect>) {
    let cmd: ControlCommand = match serde_json::from_slice(line) {
        Ok(c) => c,
        Err(err) => return (error_line(&format!("invalid command: {err}")), Vec::new()),
    };
    let effects = match cmd {
        ControlCommand::Status => Vec::new(),
        ControlCommand::Resize { capacity } => match state.resize(capacity) {
            Ok(eff) => eff,
            Err(err) => return (error_line(&err), Vec::new()),
        },
        ControlCommand::SetMaxPerOwner { max_per_owner } => {
            match state.set_max_per_owner(max_per_owner) {
                Ok(eff) => eff,
                Err(err) => return (error_line(&err), Vec::new()),
            }
        }
    };
    let mut out = serde_json::to_vec(&state.status()).unwrap_or_else(|_| error_line("serialize"));
    out.push(b'\n');
    (out, effects)
}

pub fn send_request(
    socket_path: &Path,
    payload: &serde_json::Value,
) -> io::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut req = serde_json::to_vec(payload).map_err(io::Error::other)?;
    req.push(b'\n');
    stream.write_all(&req)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty reply"));
    }
    serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
