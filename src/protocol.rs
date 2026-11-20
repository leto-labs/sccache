use crate::cache::{CacheMode, GetPathResult};
use crate::compiler::ColorMode;
use crate::config::{PreprocessorCacheModeConfig, RustIncremental};
use crate::server::{DistInfo, ServerInfo, ServerStats};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::ffi::OsString;

/// A client request.
#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    /// Zero the server's statistics.
    ZeroStats,
    /// Get server statistics.
    GetStats,
    /// Get dist status.
    DistStatus,
    /// Shut the server down gracefully.
    Shutdown,
    /// Execute a compile or fetch a cached compilation result.
    Compile(Compile),

    // --- Storage RPCs (client-side mode) ---
    /// One-shot handshake: client requests cache metadata from the daemon.
    StorageHandshake,
    /// Fetch the filesystem path of the cached entry for `key`.
    /// Returns `None` if the backend does not support direct file access or the key is absent.
    StorageGetPath { key: String },
    /// Fetch raw (zip) bytes for `key`; returns `None` on a miss.
    StorageGetRaw { key: String },
    /// Store raw (zip) bytes under `key`.
    StoragePutRaw { key: String, data: Vec<u8> },
    /// Retrieve the preprocessor cache entry for `key`.
    StorageGetPreprocessorEntry { key: String },
    /// Store or overwrite the preprocessor cache entry for `key`.
    StoragePutPreprocessorEntry { key: String, entry_bytes: Vec<u8> },
    /// Merge per-invocation stats into the daemon's running totals.
    RecordStats(Box<ServerStats>),
}

/// A server response.
#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    /// Response for `Request::Compile`.
    Compile(CompileResponse),
    /// Response for `Request::ZeroStats`.
    ZeroStats,
    /// Response for `Request::GetStats`, containing server statistics.
    Stats(Box<ServerInfo>),
    /// Response for `Request::DistStatus`, containing client info.
    DistStatus(DistInfo),
    /// Response for `Request::Shutdown`, containing server statistics.
    ShuttingDown(Box<ServerInfo>),
    /// Second response for `Request::Compile`, containing the results of the compilation.
    CompileFinished(CompileFinished),

    // --- Storage RPC responses (client-side mode) ---
    /// Response for `Request::StorageHandshake`.
    StorageHandshake(StorageHandshakeInfo),
    /// Response for `Request::StorageGetPath`.
    StorageGetPath(GetPathResult),
    /// Response for `Request::StorageGetRaw`: zip bytes on hit, `None` on miss.
    StorageGetRaw(Option<Vec<u8>>),
    /// Response for `Request::StoragePutRaw`.
    StoragePutRaw(Result<(), String>),
    /// Response for `Request::StorageGetPreprocessorEntry`.
    StorageGetPreprocessorEntry(Result<Option<Vec<u8>>, String>),
    /// Response for `Request::StoragePutPreprocessorEntry`.
    StoragePutPreprocessorEntry(Result<(), String>),
    /// Response for `Request::RecordStats`.
    RecordStats,

    /// A rustc artifact notification delivered while the compiler is still running.
    ///
    /// The line also stays in [`CompileFinished::stderr`].
    /// The client that has forwarded the notification must drop that copy.
    ///
    /// Currently only `.rmeta` notifications are sent (for Cargo pipelining).
    ArtifactNotification(Vec<u8>),
}

/// Possible responses from the server for a `Compile` request.
#[derive(Serialize, Deserialize, Debug)]
pub enum CompileResponse {
    /// The compilation was started.
    CompileStarted,
    /// The server could not handle this compilation request.
    UnhandledCompile,
    /// The compiler was not supported.
    UnsupportedCompiler(OsString),
}

/// Information about a finished compile, either from cache or executed locally.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CompileFinished {
    /// The return code of the compile process, if available.
    pub retcode: Option<i32>,
    /// The signal that terminated the compile process, if available.
    pub signal: Option<i32>,
    /// The compiler's stdout.
    pub stdout: Vec<u8>,
    /// The compiler's stderr.
    pub stderr: Vec<u8>,
    /// The state of any compiler options passed to control color output.
    pub color_mode: ColorMode,
}

/// The contents of a compile request from a client.
#[derive(Serialize, Deserialize, Debug)]
pub struct Compile {
    /// The full path to the compiler executable.
    pub exe: OsString,
    /// The current working directory in which to execute the compile.
    pub cwd: OsString,
    /// The commandline arguments passed to the compiler.
    pub args: Vec<OsString>,
    /// The environment variables present when the compiler was executed, as (var, val).
    pub env_vars: Vec<(OsString, OsString)>,
}

/// Cache metadata returned by the daemon on `StorageHandshake`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageHandshakeInfo {
    pub location: String,
    pub cache_type_name: String,
    pub basedirs: Vec<Vec<u8>>,
    pub preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
    pub cache_mode: CacheMode,
    pub max_size: Option<u64>,
    #[serde(skip)]
    pub rust_incremental: RustIncremental,
}

// Older peers ignore trailing bytes. Keep extensions at the end of the
// frame, since the legacy stats are nested inside the response payload.
pub(crate) fn encode_request(request: &Request) -> bincode::Result<Vec<u8>> {
    let mut frame = bincode::serialize(request)?;
    if let Request::RecordStats(stats) = request {
        bincode::serialize_into(&mut frame, &stats.rust_incremental_not_stored)?;
    }
    Ok(frame)
}

pub(crate) fn decode_request(frame: &[u8]) -> bincode::Result<Request> {
    let (mut request, extension) = decode_frame(frame)?;
    if let Request::RecordStats(stats) = &mut request
        && !extension.is_empty()
    {
        stats.rust_incremental_not_stored = bincode::deserialize(extension)?;
    }
    Ok(request)
}

pub(crate) fn encode_response(response: &Response) -> bincode::Result<Vec<u8>> {
    let mut frame = Vec::new();
    bincode::serialize_into(&mut frame, response)?;
    match response {
        Response::Stats(info) | Response::ShuttingDown(info) => {
            bincode::serialize_into(&mut frame, &info.stats.rust_incremental_not_stored)?;
        }
        Response::StorageHandshake(info) => {
            bincode::serialize_into(&mut frame, &info.rust_incremental)?;
        }
        _ => {}
    }
    Ok(frame)
}

pub(crate) fn decode_response(frame: &[u8]) -> bincode::Result<Response> {
    let (mut response, extension) = decode_frame(frame)?;
    if !extension.is_empty() {
        match &mut response {
            Response::Stats(info) | Response::ShuttingDown(info) => {
                info.stats.rust_incremental_not_stored = bincode::deserialize(extension)?;
            }
            Response::StorageHandshake(info) => {
                info.rust_incremental = bincode::deserialize(extension)?;
            }
            _ => {}
        }
    }
    Ok(response)
}

fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> bincode::Result<(T, &[u8])> {
    let mut remaining = frame;
    let message = bincode::deserialize_from(&mut remaining)?;
    Ok((message, remaining))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn legacy_stats_payload() -> Vec<u8> {
        let counts = (
            BTreeMap::from([("Rust", 7_u64)]),
            BTreeMap::from([("rust", 7_u64)]),
        );
        bincode::serialize(&(
            [1_u64, 2, 3, 4, 5],
            (&counts, &counts, &counts),
            [6_u64, 7, 8, 9, 10, 11],
            Duration::from_secs(12),
            Duration::from_secs(13),
            14_u64,
            Duration::from_secs(15),
            16_u64,
            BTreeMap::from([("incremental session", 17_usize)]),
            BTreeMap::from([("worker", 18_usize)]),
            19_u64,
            Option::<()>::None,
        ))
        .unwrap()
    }

    #[test]
    fn stats_and_shutdown_frames_support_legacy_peers() {
        // Enum indices for Stats and ShuttingDown in the original protocol.
        for tag in [2_u32, 4] {
            let mut old_frame = bincode::serialize(&tag).unwrap();
            old_frame.extend(legacy_stats_payload());
            bincode::serialize_into(
                &mut old_frame,
                &(
                    "cache",
                    Some(100_u64),
                    Some(200_u64),
                    true,
                    "old",
                    vec!["/work"],
                ),
            )
            .unwrap();

            let mut response = decode_response(&old_frame).unwrap();
            let (Response::Stats(info) | Response::ShuttingDown(info)) = &mut response else {
                panic!("expected stats response");
            };
            assert_eq!(info.stats.cache_writes, 11);
            assert_eq!(info.stats.not_cached["incremental session"], 17);
            assert_eq!(info.stats.rust_incremental_not_stored, 0);
            assert_eq!(info.cache_location, "cache");
            assert_eq!(info.basedirs, ["/work"]);

            info.stats.rust_incremental_not_stored = 23;
            let json = serde_json::to_value(&info).unwrap();
            assert_eq!(json["stats"]["rust_incremental_not_stored"], 23);
            let from_json: ServerInfo = serde_json::from_value(json).unwrap();
            assert_eq!(from_json.stats.rust_incremental_not_stored, 23);

            let new_frame = encode_response(&response).unwrap();
            assert!(new_frame.starts_with(&old_frame));
            for (response, expected) in [
                (bincode::deserialize(&new_frame).unwrap(), 0),
                (decode_response(&new_frame).unwrap(), 23),
            ] {
                let (Response::Stats(info) | Response::ShuttingDown(info)) = response else {
                    panic!("expected stats response");
                };
                assert_eq!(info.stats.rust_incremental_not_stored, expected);
                assert_eq!(info.cache_location, "cache");
            }
        }
    }

    #[test]
    fn record_stats_frames_support_legacy_peers() {
        let mut old_frame = bincode::serialize(&11_u32).unwrap(); // RecordStats
        old_frame.extend(legacy_stats_payload());
        let Request::RecordStats(mut stats) = decode_request(&old_frame).unwrap() else {
            panic!("expected stats request");
        };
        assert_eq!(stats.cache_writes, 11);
        assert_eq!(stats.rust_incremental_not_stored, 0);
        stats.rust_incremental_not_stored = 23;
        let new_frame = encode_request(&Request::RecordStats(stats)).unwrap();
        assert!(new_frame.starts_with(&old_frame));
        for (request, expected) in [
            (bincode::deserialize(&new_frame).unwrap(), 0),
            (decode_request(&new_frame).unwrap(), 23),
        ] {
            let Request::RecordStats(stats) = request else {
                panic!("expected stats request");
            };
            assert_eq!(stats.rust_incremental_not_stored, expected);
            assert_eq!(stats.cache_writes, 11);
        }
    }

    #[test]
    fn storage_handshake_supports_legacy_peers() {
        let old_frame = bincode::serialize(&(
            6_u32, // StorageHandshake
            "cache",
            "disk",
            vec![b"/work/".to_vec()],
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadOnly,
            Some(200_u64),
        ))
        .unwrap();

        let Response::StorageHandshake(mut info) = decode_response(&old_frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert_eq!(info.rust_incremental, RustIncremental::Off);
        assert_eq!(info.cache_mode, CacheMode::ReadOnly);
        assert_eq!(info.max_size, Some(200));
        info.rust_incremental = RustIncremental::CacheNoBypass;
        let new_frame = encode_response(&Response::StorageHandshake(info)).unwrap();
        assert!(new_frame.starts_with(&old_frame));
        for (response, expected) in [
            (
                bincode::deserialize(&new_frame).unwrap(),
                RustIncremental::Off,
            ),
            (
                decode_response(&new_frame).unwrap(),
                RustIncremental::CacheNoBypass,
            ),
        ] {
            let Response::StorageHandshake(info) = response else {
                panic!("expected storage handshake");
            };
            assert_eq!(info.rust_incremental, expected);
            assert_eq!(info.cache_mode, CacheMode::ReadOnly);
        }
    }
}
