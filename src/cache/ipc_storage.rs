// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::cache::CacheMode;
use crate::cache::GetPathResult;
use crate::cache::cache_io::{Cache, CacheRead, CacheWrite, OpenCacheObject};
use crate::cache::storage::{CacheStorage, PreprocessorCacheStorage, put_packed_objects};
use crate::client::ServerConnection;
use crate::compiler::PreprocessorCacheEntry;
use crate::config::{
    PreprocessorCacheModeConfig, RustCacheLinked, RustHashCargoEnv, RustIncrementalSettings,
};
use crate::errors::*;
use crate::protocol::{Request, Response, StorageHandshakeInfo};
use async_trait::async_trait;
use bytes::Bytes;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `Storage` implementation that forwards all cache operations to the sccache
/// daemon over the existing IPC connection.  Used by CLI processes in
/// client-side mode.
///
/// `ServerConnection` is synchronous and non-`Clone`, so it lives behind a
/// `Mutex` and every RPC dispatches via `tokio::task::spawn_blocking`.  The
/// lock is held only for the duration of a single blocking call, never across
/// an `.await` point.
pub struct IpcStorage {
    conn: Arc<Mutex<ServerConnection>>,
    handshake: StorageHandshakeInfo,
}

impl IpcStorage {
    pub fn new(conn: ServerConnection, handshake: StorageHandshakeInfo) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            handshake,
        }
    }

    /// Return a clone of the underlying connection handle so callers can send
    /// additional RPCs (e.g., `RecordStats`) after the storage is no longer
    /// needed.
    pub fn conn(&self) -> Arc<Mutex<ServerConnection>> {
        Arc::clone(&self.conn)
    }

    pub fn rust_incremental(&self) -> RustIncrementalSettings {
        self.handshake.rust_incremental
    }

    pub(crate) fn rust_cache_linked_max_entry_bytes(&self) -> u64 {
        self.handshake.rust_cache_linked_max_entry_bytes
    }

    pub(crate) fn rust_cache_linked(&self) -> RustCacheLinked {
        self.handshake.rust_cache_linked
    }

    pub(crate) fn rust_hash_cargo_env(&self) -> RustHashCargoEnv {
        self.handshake.rust_hash_cargo_env
    }

    pub(crate) fn incremental_prune_days(&self) -> u64 {
        self.handshake.incremental_prune_days
    }

    async fn rpc(&self, req: Request) -> Result<Response> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || conn.lock().unwrap().request(req))
            .await
            .context("spawn_blocking panicked")?
    }
}

#[async_trait]
impl CacheStorage for IpcStorage {
    async fn get(&self, key: &str) -> Result<Cache> {
        match self.get_path(key).await {
            GetPathResult::Found(path) => {
                let file = fs_err::File::open(&path)
                    .with_context(|| format!("IpcStorage::get: open {}", path.display()))?;
                match CacheRead::from_file(file, self.write_raw_objects()) {
                    Ok(entry) => Ok(Cache::Hit(entry)),
                    Err(_) => Ok(Cache::Miss),
                }
            }
            GetPathResult::Miss => Ok(Cache::Miss),
            // Backend doesn't support paths (S3, Redis, …); fall back to bytes over IPC.
            GetPathResult::Unsupported => match self.get_raw(key).await? {
                Some(bytes) => match CacheRead::from(Cursor::new(bytes)) {
                    Ok(entry) => Ok(Cache::Hit(entry)),
                    Err(_) => Ok(Cache::Miss),
                },
                None => Ok(Cache::Miss),
            },
        }
    }

    async fn get_path(&self, key: &str) -> GetPathResult {
        match self
            .rpc(Request::StorageGetPath {
                key: key.to_owned(),
            })
            .await
        {
            Ok(Response::StorageGetPath(result)) => result,
            _ => GetPathResult::Unsupported,
        }
    }

    async fn put(&self, key: &str, entry: CacheWrite) -> Result<Duration> {
        self.put_raw(key, entry.finish()?.into()).await
    }

    async fn put_objects(
        &self,
        key: &str,
        objects: Vec<OpenCacheObject>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        pool: &tokio::runtime::Handle,
        _staged: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<Duration> {
        #[cfg(target_os = "linux")]
        if self.handshake.put_by_path && !objects.is_empty() {
            // Missing optional members were already skipped when opening the outputs.
            if let Ok(sources) = objects.iter().map(OpenCacheObject::store_object).collect() {
                let start = Instant::now();
                match self
                    .rpc(Request::StoragePutByPath {
                        key: key.to_owned(),
                        objects: sources,
                        stdout: stdout.clone(),
                        stderr: stderr.clone(),
                    })
                    .await?
                {
                    Response::StoragePutByPath(Ok(true)) => return Ok(start.elapsed()),
                    Response::StoragePutByPath(Ok(false)) => (),
                    Response::StoragePutByPath(Err(error)) => {
                        bail!("IpcStorage::put_by_path: {error}")
                    }
                    other => bail!("IpcStorage::put_by_path: unexpected response: {other:?}"),
                }
            }
        }
        put_packed_objects(self, key, objects, stdout, stderr, pool).await
    }

    fn write_raw_objects(&self) -> bool {
        self.handshake.write_raw_objects
    }

    async fn get_raw(&self, key: &str) -> Result<Option<Bytes>> {
        let resp = self
            .rpc(Request::StorageGetRaw {
                key: key.to_owned(),
            })
            .await?;
        match resp {
            Response::StorageGetRaw(opt) => Ok(opt.map(Bytes::from)),
            other => bail!("IpcStorage::get_raw: unexpected response: {other:?}"),
        }
    }

    async fn put_raw(&self, key: &str, data: Bytes) -> Result<Duration> {
        let start = Instant::now();
        let resp = self
            .rpc(Request::StoragePutRaw {
                key: key.to_owned(),
                data: data.to_vec(),
            })
            .await?;
        match resp {
            Response::StoragePutRaw(Ok(())) => Ok(start.elapsed()),
            Response::StoragePutRaw(Err(e)) => bail!("IpcStorage::put_raw: daemon error: {e}"),
            other => bail!("IpcStorage::put_raw: unexpected response: {other:?}"),
        }
    }

    async fn check(&self) -> Result<CacheMode> {
        Ok(self.handshake.cache_mode)
    }

    fn location(&self) -> String {
        self.handshake.location.clone()
    }

    fn cache_type_name(&self) -> &'static str {
        "ipc"
    }

    async fn current_size(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(self.handshake.max_size)
    }

    fn basedirs(&self) -> &[Vec<u8>] {
        &self.handshake.basedirs
    }
}

#[async_trait]
impl PreprocessorCacheStorage for IpcStorage {
    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        self.handshake.preprocessor_cache_mode_config
    }

    async fn get_preprocessor_cache_entry(
        &self,
        key: &str,
    ) -> Result<Option<Box<dyn crate::lru_disk_cache::ReadSeek>>> {
        let resp = self
            .rpc(Request::StorageGetPreprocessorEntry {
                key: key.to_owned(),
            })
            .await?;
        match resp {
            Response::StorageGetPreprocessorEntry(Ok(None)) => Ok(None),
            Response::StorageGetPreprocessorEntry(Ok(Some(bytes))) => Ok(Some(
                Box::new(Cursor::new(bytes)) as Box<dyn crate::lru_disk_cache::ReadSeek>,
            )),
            Response::StorageGetPreprocessorEntry(Err(e)) => {
                bail!("IpcStorage::get_preprocessor_cache_entry: {e}")
            }
            other => {
                bail!("IpcStorage::get_preprocessor_cache_entry: unexpected response: {other:?}")
            }
        }
    }

    async fn put_preprocessor_cache_entry(
        &self,
        key: &str,
        entry: PreprocessorCacheEntry,
    ) -> Result<()> {
        let mut buf = vec![];
        entry
            .serialize_to(&mut buf)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let resp = self
            .rpc(Request::StoragePutPreprocessorEntry {
                key: key.to_owned(),
                entry_bytes: buf,
            })
            .await?;
        match resp {
            Response::StoragePutPreprocessorEntry(Ok(())) => Ok(()),
            Response::StoragePutPreprocessorEntry(Err(e)) => {
                bail!("IpcStorage::put_preprocessor_cache_entry: {e}")
            }
            other => {
                bail!("IpcStorage::put_preprocessor_cache_entry: unexpected response: {other:?}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FileObjectSource;
    use crate::protocol::{decode_request, encode_response};
    use crate::util::write_length_prefixed_bytes;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};

    #[test_case::test_case(false, false; "legacy handshake")]
    #[test_case::test_case(true, false; "raw handshake")]
    #[cfg_attr(target_os = "linux", test_case::test_case(true, true; "clone failure"))]
    fn ipc_packs_objects_in_daemon_format(raw: bool, put_by_path: bool) {
        fn read_request(mut stream: &TcpStream) -> Request {
            let mut length = [0; 4];
            stream.read_exact(&mut length).unwrap();
            let mut frame = vec![0; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut frame).unwrap();
            decode_request(&frame).unwrap()
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let daemon = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert!(matches!(read_request(&stream), Request::StorageHandshake));
            let mut frame = encode_response(&Response::StorageHandshake(StorageHandshakeInfo {
                location: "cache".into(),
                cache_type_name: "disk".into(),
                basedirs: vec![],
                preprocessor_cache_mode_config: Default::default(),
                cache_mode: CacheMode::ReadWrite,
                max_size: None,
                rust_incremental: Default::default(),
                rust_cache_linked: Default::default(),
                rust_hash_cargo_env: RustHashCargoEnv::All,
                rust_cache_linked_max_entry_bytes:
                    crate::config::default_rust_cache_linked_max_entry_bytes(),
                miss_explain: false,
                incremental_prune_days: 7,
                write_raw_objects: true,
                put_by_path,
            }))
            .unwrap();
            if !raw {
                frame.pop();
            }
            write_length_prefixed_bytes(&stream, &frame).unwrap();
            if put_by_path {
                let Request::StoragePutByPath { objects, .. } = read_request(&stream) else {
                    panic!("expected path store");
                };
                assert_eq!(objects[0].key, "object");
                assert_eq!(objects[0].size, b"compiler output".len() as u64);
                let response = encode_response(&Response::StoragePutByPath(Ok(false))).unwrap();
                write_length_prefixed_bytes(&stream, &response).unwrap();
            }
            let Request::StoragePutRaw { data, .. } = read_request(&stream) else {
                panic!("expected a cache put");
            };
            let zip = zip::ZipArchive::new(Cursor::new(&data)).unwrap();
            let member = if raw { "raw/object" } else { "object" };
            assert!(zip.index_for_name(member).is_some());
            assert_eq!(zip.len(), 1);
            let mut entry = CacheRead::from(Cursor::new(data)).unwrap();
            let mut bytes = Vec::new();
            entry.get_object("object", &mut bytes).unwrap();
            assert_eq!(bytes, b"compiler output");
            let response = encode_response(&Response::StoragePutRaw(Ok(()))).unwrap();
            write_length_prefixed_bytes(&stream, &response).unwrap();
        });
        let mut conn = ServerConnection::new(Box::new(stream)).unwrap();
        let Response::StorageHandshake(handshake) =
            conn.request(Request::StorageHandshake).unwrap()
        else {
            panic!("expected a storage handshake");
        };
        let storage = IpcStorage::new(conn, handshake);
        let output = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(output.path(), b"compiler output").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            storage
                .put_objects(
                    "01",
                    CacheWrite::open_objects([FileObjectSource {
                        key: "object".into(),
                        path: output.path().into(),
                        optional: false,
                    }])
                    .unwrap(),
                    vec![],
                    vec![],
                    runtime.handle(),
                    None,
                )
                .await
                .unwrap();
        });
        daemon.join().unwrap();
    }
}
