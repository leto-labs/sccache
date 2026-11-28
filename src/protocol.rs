use crate::cache::{CacheMode, GetPathResult};
use crate::compile_queue::{CompileQueueStats, CompileStage, WaitStats};
use crate::compiler::ColorMode;
use crate::config::{
    PreprocessorCacheModeConfig, RustCacheLinked, RustHashCargoEnv, RustIncrementalFirstBuild,
    RustIncrementalSettings, default_incremental_prune_days,
    default_rust_cache_linked_max_entry_bytes,
};
use crate::server::{DistInfo, ServerInfo, ServerStats};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use std::ffi::OsString;

// Bincode's fixed-width, little-endian discriminant for StoragePutRaw.
// The server checks this before reading the body.
pub(crate) const STORAGE_PUT_RAW_TAG: [u8; 4] = 8_u32.to_le_bytes();

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
    /// Store a compilation by cloning its outputs from local files.
    StoragePutByPath {
        key: String,
        objects: Vec<StoreObject>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StoreObject {
    pub key: String,
    pub path: OsString,
    pub device: u64,
    pub inode: u64,
    pub size: u64,
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
    /// `Ok(false)` requests the usual raw-entry fallback.
    StoragePutByPath(Result<bool, String>),
    /// `Request::Shutdown` refused: `[server] allow_client_shutdown = false`.
    ShutdownRefused(String),
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
    pub rust_incremental: RustIncrementalSettings,
    #[serde(skip)]
    pub miss_explain: bool,
    #[serde(skip, default = "default_incremental_prune_days")]
    pub incremental_prune_days: u64,
    #[serde(skip)]
    pub write_raw_objects: bool,
    #[serde(skip)]
    pub rust_cache_linked: RustCacheLinked,
    #[serde(skip, default = "default_rust_cache_linked_max_entry_bytes")]
    pub rust_cache_linked_max_entry_bytes: u64,
    #[serde(skip, default = "legacy_rust_hash_cargo_env")]
    pub rust_hash_cargo_env: RustHashCargoEnv,
    #[serde(skip)]
    pub put_by_path: bool,
}

fn legacy_rust_hash_cargo_env() -> RustHashCargoEnv {
    RustHashCargoEnv::All
}

// Older peers ignore trailing bytes. Keep extensions at the end of the
// frame, since the legacy stats are nested inside the response payload.
pub(crate) fn encode_request(request: &Request) -> bincode::Result<Vec<u8>> {
    let mut frame = bincode::serialize(request)?;
    if let Request::RecordStats(stats) = request {
        encode_stats_extension(&mut frame, stats, false)?;
    }
    Ok(frame)
}

pub(crate) fn decode_request(frame: &[u8]) -> bincode::Result<Request> {
    let (mut request, extension) = decode_frame(frame)?;
    if let Request::RecordStats(stats) = &mut request
        && !extension.is_empty()
    {
        decode_stats_extension(extension, stats)?;
    }
    Ok(request)
}

pub(crate) fn encode_response(response: &Response) -> bincode::Result<Vec<u8>> {
    let mut frame = Vec::new();
    bincode::serialize_into(&mut frame, response)?;
    match response {
        Response::Stats(info) | Response::ShuttingDown(info) => {
            encode_stats_extension(&mut frame, &info.stats, true)?;
        }
        Response::StorageHandshake(info) => {
            bincode::serialize_into(&mut frame, &info.rust_incremental.mode)?;
            bincode::serialize_into(&mut frame, &info.miss_explain)?;
            bincode::serialize_into(&mut frame, &info.incremental_prune_days)?;
            bincode::serialize_into(&mut frame, &info.rust_incremental.first_build)?;
            bincode::serialize_into(&mut frame, &info.write_raw_objects)?;
            let custom_limit = info.rust_cache_linked_max_entry_bytes
                != default_rust_cache_linked_max_entry_bytes();
            let semantic_env = info.rust_hash_cargo_env != legacy_rust_hash_cargo_env();
            if info.rust_cache_linked != RustCacheLinked::Off
                || custom_limit
                || semantic_env
                || info.put_by_path
            {
                bincode::serialize_into(&mut frame, &info.rust_cache_linked)?;
                if custom_limit || semantic_env || info.put_by_path {
                    bincode::serialize_into(&mut frame, &info.rust_cache_linked_max_entry_bytes)?;
                }
                if semantic_env || info.put_by_path {
                    bincode::serialize_into(&mut frame, &info.rust_hash_cargo_env)?;
                }
                if info.put_by_path {
                    bincode::serialize_into(&mut frame, &info.put_by_path)?;
                }
            }
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
                let remaining = decode_stats_extension(extension, &mut info.stats)?;
                if !remaining.is_empty() {
                    let (queue, remaining) = decode_frame::<
                        CompileQueueWire<BTreeMap<CompileStage, WaitStats>>,
                    >(remaining)?;
                    info.stats.compile_queue = Some(CompileQueueStats {
                        token_capacity: queue.token_capacity,
                        running: queue.running,
                        token_wait: queue.token_wait,
                        token_waits_over_10s: queue.token_waits_over_10s,
                        stages: queue.stages,
                        tokens_in_use: None,
                        stalled_for: None,
                        pool_swaps: None,
                        retire_pending: None,
                    });
                    let queue = info.stats.compile_queue.as_mut().unwrap();
                    // Each later field is appended; older daemons stop sooner.
                    if !remaining.is_empty() {
                        let (tokens_in_use, remaining) = decode_frame(remaining)?;
                        queue.tokens_in_use = Some(tokens_in_use);
                        // -11 sent a restored-token count here; later daemons send 0.
                        if !remaining.is_empty() {
                            let (_, remaining) = decode_frame::<u64>(remaining)?;
                            if !remaining.is_empty() {
                                let (stalled_for, remaining) = decode_frame(remaining)?;
                                queue.stalled_for = Some(stalled_for);
                                if !remaining.is_empty() {
                                    let (pool_swaps, remaining) = decode_frame(remaining)?;
                                    queue.pool_swaps = Some(pool_swaps);
                                    if !remaining.is_empty() {
                                        queue.retire_pending =
                                            Some(bincode::deserialize(remaining)?);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Response::StorageHandshake(info) => {
                let (mode, remaining) = decode_frame(extension)?;
                info.rust_incremental = RustIncrementalSettings {
                    mode,
                    // Peers without the policy compiled all misses incrementally.
                    first_build: RustIncrementalFirstBuild::Incremental,
                };
                if !remaining.is_empty() {
                    let (miss_explain, remaining) = decode_frame(remaining)?;
                    info.miss_explain = miss_explain;
                    if !remaining.is_empty() {
                        let (prune_days, remaining) = decode_frame(remaining)?;
                        info.incremental_prune_days = prune_days;
                        if !remaining.is_empty() {
                            let (first_build, remaining) = decode_frame(remaining)?;
                            info.rust_incremental.first_build = first_build;
                            if !remaining.is_empty() {
                                let (write_raw_objects, remaining) = decode_frame(remaining)?;
                                info.write_raw_objects = write_raw_objects;
                                if !remaining.is_empty() {
                                    let (linked, remaining) = decode_frame(remaining)?;
                                    info.rust_cache_linked = linked;
                                    if !remaining.is_empty() {
                                        let (limit, remaining) = decode_frame(remaining)?;
                                        info.rust_cache_linked_max_entry_bytes = limit;
                                        if !remaining.is_empty() {
                                            let (hash_env, remaining) = decode_frame(remaining)?;
                                            info.rust_hash_cargo_env = hash_env;
                                            if !remaining.is_empty() {
                                                info.put_by_path = bincode::deserialize(remaining)?;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(response)
}

// The queue extension predates the token gauge. Keep its original prefix so
// readers that stop after the queue can still decode it.
#[derive(Deserialize, Serialize)]
struct CompileQueueWire<Stages> {
    token_capacity: usize,
    running: usize,
    token_wait: WaitStats,
    token_waits_over_10s: u64,
    stages: Stages,
}

fn encode_stats_extension(
    frame: &mut Vec<u8>,
    stats: &ServerStats,
    include_queue: bool,
) -> bincode::Result<()> {
    let queue = stats.compile_queue.as_ref().filter(|_| include_queue);
    bincode::serialize_into(&mut *frame, &stats.rust_incremental_not_stored)?;
    if stats.miss_explain != Default::default()
        || stats.rust_linked_entry_too_large != 0
        || queue.is_some()
    {
        bincode::serialize_into(&mut *frame, &stats.miss_explain)?;
    }
    if stats.rust_linked_entry_too_large != 0 || queue.is_some() {
        bincode::serialize_into(&mut *frame, &stats.rust_linked_entry_too_large)?;
    }
    if let Some(queue) = queue {
        bincode::serialize_into(
            &mut *frame,
            &CompileQueueWire {
                token_capacity: queue.token_capacity,
                running: queue.running,
                token_wait: queue.token_wait,
                token_waits_over_10s: queue.token_waits_over_10s,
                stages: &queue.stages,
            },
        )?;
        if let Some(tokens_in_use) = queue.tokens_in_use {
            bincode::serialize_into(&mut *frame, &tokens_in_use)?;
            // Older decoders read tokens_in_use as the last field and ignore the rest.
            if let Some(stalled_for) = queue.stalled_for {
                bincode::serialize_into(&mut *frame, &0_u64)?;
                bincode::serialize_into(&mut *frame, &stalled_for)?;
                if let (Some(pool_swaps), Some(retire_pending)) =
                    (queue.pool_swaps, queue.retire_pending)
                {
                    bincode::serialize_into(&mut *frame, &pool_swaps)?;
                    bincode::serialize_into(frame, &retire_pending)?;
                }
            }
        }
    }
    Ok(())
}

fn decode_stats_extension<'a>(
    extension: &'a [u8],
    stats: &mut ServerStats,
) -> bincode::Result<&'a [u8]> {
    let (incremental, mut remaining) = decode_frame(extension)?;
    stats.rust_incremental_not_stored = incremental;
    if !remaining.is_empty() {
        (stats.miss_explain, remaining) = decode_frame(remaining)?;
    }
    if !remaining.is_empty() {
        (stats.rust_linked_entry_too_large, remaining) = decode_frame(remaining)?;
    }
    Ok(remaining)
}

fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> bincode::Result<(T, &[u8])> {
    let mut remaining = frame;
    let message = bincode::deserialize_from(&mut remaining)?;
    Ok((message, remaining))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::MissExplainStats;
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
            assert_eq!(info.stats.miss_explain, MissExplainStats::default());
            assert_eq!(info.stats.rust_linked_entry_too_large, 0);
            assert_eq!(info.cache_location, "cache");
            assert_eq!(info.basedirs, ["/work"]);

            let mut incremental_frame = old_frame.clone();
            bincode::serialize_into(&mut incremental_frame, &23_u64).unwrap();
            let (Response::Stats(previous) | Response::ShuttingDown(previous)) =
                decode_response(&incremental_frame).unwrap()
            else {
                panic!("expected stats response");
            };
            assert_eq!(previous.stats.rust_incremental_not_stored, 23);
            assert_eq!(previous.stats.miss_explain, MissExplainStats::default());

            info.stats.rust_incremental_not_stored = 23;
            info.stats.miss_explain.own_input = 41;
            info.stats.rust_linked_entry_too_large = 47;
            let json = serde_json::to_value(&info).unwrap();
            assert_eq!(json["stats"]["rust_incremental_not_stored"], 23);
            assert_eq!(json["stats"]["miss_explain"]["own-input"], 41);
            assert_eq!(json["stats"]["rust_linked_entry_too_large"], 47);
            let from_json: ServerInfo = serde_json::from_value(json).unwrap();
            assert_eq!(from_json.stats.rust_incremental_not_stored, 23);
            assert_eq!(from_json.stats.miss_explain.own_input, 41);
            assert_eq!(from_json.stats.rust_linked_entry_too_large, 47);

            let new_frame = encode_response(&response).unwrap();
            assert!(new_frame.starts_with(&incremental_frame));
            for (response, incremental, own_input, oversized) in [
                (bincode::deserialize(&new_frame).unwrap(), 0, 0, 0),
                (
                    decode_response(&new_frame[..new_frame.len() - 8]).unwrap(),
                    23,
                    41,
                    0,
                ),
                (decode_response(&new_frame).unwrap(), 23, 41, 47),
            ] {
                let (Response::Stats(info) | Response::ShuttingDown(info)) = response else {
                    panic!("expected stats response");
                };
                assert_eq!(info.stats.rust_incremental_not_stored, incremental);
                assert_eq!(info.stats.miss_explain.own_input, own_input);
                assert_eq!(info.stats.rust_linked_entry_too_large, oversized);
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
        let incremental_frame = encode_request(&Request::RecordStats(stats.clone())).unwrap();
        let Request::RecordStats(previous) = decode_request(&incremental_frame).unwrap() else {
            panic!("expected stats request");
        };
        assert_eq!(previous.rust_incremental_not_stored, 23);
        assert_eq!(previous.miss_explain, MissExplainStats::default());

        stats.rust_linked_entry_too_large = 31;
        let new_frame = encode_request(&Request::RecordStats(stats)).unwrap();
        assert!(incremental_frame.starts_with(&old_frame));
        assert!(new_frame.starts_with(&incremental_frame));
        for (request, incremental, oversized) in [
            (bincode::deserialize(&new_frame).unwrap(), 0, 0),
            (decode_request(&new_frame).unwrap(), 23, 31),
        ] {
            let Request::RecordStats(stats) = request else {
                panic!("expected stats request");
            };
            assert_eq!(stats.rust_incremental_not_stored, incremental);
            assert_eq!(stats.miss_explain, MissExplainStats::default());
            assert_eq!(stats.rust_linked_entry_too_large, oversized);
            assert_eq!(stats.cache_writes, 11);
        }
    }

    #[tokio::test]
    async fn compile_queue_stats_are_optional_in_json_and_on_the_wire() {
        let mut info = ServerInfo::new(ServerStats::default(), None).await.unwrap();
        let old_json = serde_json::to_value(&info).unwrap();
        assert!(old_json["stats"].get("compile_queue").is_none());
        let decoded: ServerInfo = serde_json::from_value(old_json).unwrap();
        assert_eq!(decoded.stats.compile_queue, None);
        let old_frame = encode_response(&Response::Stats(Box::new(info.clone()))).unwrap();
        let Response::Stats(decoded) = decode_response(&old_frame).unwrap() else {
            panic!("expected stats response");
        };
        assert_eq!(decoded.stats.compile_queue, None);

        let queue = CompileQueueStats {
            token_capacity: 8,
            running: 7,
            token_wait: WaitStats {
                total: Duration::from_secs(37),
                max: Duration::from_secs(35),
                active: 2,
                oldest: Duration::from_secs(14),
            },
            token_waits_over_10s: 1,
            stages: BTreeMap::from([(CompileStage::CacheLookup, WaitStats::default())]),
            tokens_in_use: None,
            stalled_for: None,
            pool_swaps: None,
            retire_pending: None,
        };
        info.stats.compile_queue = Some(queue.clone());
        let old_json = serde_json::to_value(&info).unwrap();
        assert!(
            old_json["stats"]["compile_queue"]
                .get("tokens_in_use")
                .is_none()
        );
        assert!(
            old_json["stats"]["compile_queue"]
                .get("pool_swaps")
                .is_none()
        );
        assert!(
            old_json["stats"]["compile_queue"]
                .get("retire_pending")
                .is_none()
        );
        let decoded: ServerInfo = serde_json::from_value(old_json).unwrap();
        assert_eq!(decoded.stats.compile_queue.as_ref(), Some(&queue));
        for response in [
            Response::Stats(Box::new(info.clone())),
            Response::ShuttingDown(Box::new(info.clone())),
        ] {
            let ungauged = encode_response(&response).unwrap();
            let (Response::Stats(decoded) | Response::ShuttingDown(decoded)) =
                decode_response(&ungauged).unwrap()
            else {
                panic!("expected stats response");
            };
            assert_eq!(decoded.stats.compile_queue.as_ref(), Some(&queue));
        }
        let old_queue_frame = encode_response(&Response::Stats(Box::new(info.clone()))).unwrap();

        // A -10 daemon: the frame ends at tokens_in_use, and later fields stay
        // absent, also in the JSON that the health report reads.
        let tens_queue = CompileQueueStats {
            tokens_in_use: Some(8),
            ..queue.clone()
        };
        let mut tens_frame = old_queue_frame.clone();
        tens_frame.extend_from_slice(&8_u64.to_le_bytes());
        let Response::Stats(decoded) = decode_response(&tens_frame).unwrap() else {
            panic!("expected stats response");
        };
        assert_eq!(decoded.stats.compile_queue.as_ref(), Some(&tens_queue));
        let tens_json = serde_json::to_value(&decoded).unwrap();
        assert!(
            tens_json["stats"]["compile_queue"]
                .get("stalled_for")
                .is_none()
        );
        assert!(
            tens_json["stats"]["compile_queue"]
                .get("pool_swaps")
                .is_none()
        );
        // A -11 daemon also sent its restored-token count.
        let mut elevens_frame = tens_frame.clone();
        elevens_frame.extend_from_slice(&3_u64.to_le_bytes());
        let Response::Stats(decoded) = decode_response(&elevens_frame).unwrap() else {
            panic!("expected stats response");
        };
        assert_eq!(decoded.stats.compile_queue.as_ref(), Some(&tens_queue));

        // A -12 daemon sent the 0 placeholder and stalled_for, stopping before
        // pool_swaps and retire_pending.
        let twelves_queue = CompileQueueStats {
            tokens_in_use: Some(8),
            stalled_for: Some(Duration::from_secs(61)),
            ..queue.clone()
        };
        let mut twelves_frame = tens_frame.clone();
        twelves_frame.extend_from_slice(&0_u64.to_le_bytes());
        twelves_frame.extend_from_slice(&61_u64.to_le_bytes());
        twelves_frame.extend_from_slice(&0_u32.to_le_bytes());
        let Response::Stats(decoded) = decode_response(&twelves_frame).unwrap() else {
            panic!("expected stats response");
        };
        assert_eq!(decoded.stats.compile_queue.as_ref(), Some(&twelves_queue));
        let twelves_json = serde_json::to_value(&decoded).unwrap();
        assert!(
            twelves_json["stats"]["compile_queue"]
                .get("pool_swaps")
                .is_none()
        );
        assert!(
            twelves_json["stats"]["compile_queue"]
                .get("retire_pending")
                .is_none()
        );

        let queue = CompileQueueStats {
            tokens_in_use: Some(8),
            stalled_for: Some(Duration::from_secs(61)),
            pool_swaps: Some(3),
            retire_pending: Some(1),
            ..queue
        };
        info.stats.compile_queue = Some(queue.clone());
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["stats"]["compile_queue"]["token_wait"]["active"], 2);
        assert_eq!(json["stats"]["compile_queue"]["tokens_in_use"], 8);
        assert_eq!(json["stats"]["compile_queue"]["pool_swaps"], 3);
        assert_eq!(json["stats"]["compile_queue"]["retire_pending"], 1);
        let decoded: ServerInfo = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.stats.compile_queue.as_ref(), Some(&queue));

        for response in [
            Response::Stats(Box::new(info.clone())),
            Response::ShuttingDown(Box::new(info.clone())),
        ] {
            let frame = encode_response(&response).unwrap();
            let (Response::Stats(decoded) | Response::ShuttingDown(decoded)) =
                decode_response(&frame).unwrap()
            else {
                panic!("expected stats response");
            };
            assert_eq!(decoded.stats.compile_queue.as_ref(), Some(&queue));
            let (_, extension) = decode_frame::<Response>(&frame).unwrap();
            let (_, remaining) = decode_frame::<u64>(extension).unwrap();
            let (_, remaining) = decode_frame::<MissExplainStats>(remaining).unwrap();
            // Older decoders read their last field and ignore trailing bytes.
            assert_eq!(bincode::deserialize::<u64>(remaining).unwrap(), 0);
            let (_, remaining) = decode_frame::<u64>(remaining).unwrap();
            let legacy_queue: CompileQueueWire<BTreeMap<CompileStage, WaitStats>> =
                bincode::deserialize(remaining).unwrap();
            assert_eq!(legacy_queue.running, 7);
            assert_eq!(legacy_queue.stages, queue.stages);
            let (_, rest) =
                decode_frame::<CompileQueueWire<BTreeMap<CompileStage, WaitStats>>>(remaining)
                    .unwrap();
            // -10 decoder: tokens_in_use last.
            assert_eq!(bincode::deserialize::<usize>(rest).unwrap(), 8);
            let (_, rest) = decode_frame::<usize>(rest).unwrap();
            // -11 decoder: restored-token placeholder (0).
            assert_eq!(bincode::deserialize::<u64>(rest).unwrap(), 0);
            let (_, rest) = decode_frame::<u64>(rest).unwrap();
            // -12 decoder: stalled_for last, ignoring pool_swaps and retire_pending.
            assert_eq!(
                bincode::deserialize::<Duration>(rest).unwrap(),
                Duration::from_secs(61)
            );
            if matches!(response, Response::Stats(_)) {
                let mut suffix = 3_u64.to_le_bytes().to_vec();
                suffix.extend_from_slice(&1_u64.to_le_bytes());
                assert_eq!(
                    frame.strip_suffix(suffix.as_slice()),
                    Some(twelves_frame.as_slice())
                );
            }
        }

        let mut delta = info.stats.clone();
        delta.cache_writes = 3;
        let frame = encode_request(&Request::RecordStats(Box::new(delta.clone()))).unwrap();
        let Request::RecordStats(decoded) = decode_request(&frame).unwrap() else {
            panic!("expected stats delta");
        };
        assert_eq!(decoded.compile_queue, None);
        let mut daemon = ServerStats::default();
        daemon += delta;
        assert_eq!(daemon.cache_writes, 3);
        assert_eq!(daemon.compile_queue, None);
    }

    #[test]
    fn storage_handshake_supports_legacy_peers() {
        use crate::config::RustIncremental::{CacheNoBypass, Off};
        use RustIncrementalFirstBuild::{Incremental, Plain};

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
        assert_eq!(info.rust_incremental.mode, Off);
        assert_eq!(info.rust_incremental.first_build, Plain);
        assert_eq!(info.incremental_prune_days, 7);
        assert_eq!(info.cache_mode, CacheMode::ReadOnly);
        assert_eq!(info.max_size, Some(200));
        assert!(!info.miss_explain);
        assert!(!info.write_raw_objects);
        assert_eq!(info.rust_cache_linked, RustCacheLinked::Off);
        assert_eq!(info.rust_cache_linked_max_entry_bytes, 256 * 1024 * 1024);
        assert_eq!(info.rust_hash_cargo_env, RustHashCargoEnv::All);
        assert!(!info.put_by_path);

        let mode = CacheNoBypass;
        let mut mode_only = old_frame.clone();
        bincode::serialize_into(&mut mode_only, &mode).unwrap();
        let mut explain_only = mode_only.clone();
        bincode::serialize_into(&mut explain_only, &true).unwrap();
        let mut explain_and_prune = explain_only.clone();
        bincode::serialize_into(&mut explain_and_prune, &0_u64).unwrap();

        info.rust_incremental.mode = mode;
        info.miss_explain = true;
        info.incremental_prune_days = 0;
        let plain_frame = encode_response(&Response::StorageHandshake(info.clone())).unwrap();
        assert!(plain_frame.starts_with(&explain_and_prune));

        info.rust_incremental.first_build = Incremental;
        info.miss_explain = false;
        info.incremental_prune_days = 7;
        info.write_raw_objects = true;
        let mut legacy_frame = mode_only.clone();
        bincode::serialize_into(&mut legacy_frame, &false).unwrap();
        bincode::serialize_into(&mut legacy_frame, &7_u64).unwrap();
        bincode::serialize_into(&mut legacy_frame, &Incremental).unwrap();
        let mut raw_frame = legacy_frame.clone();
        bincode::serialize_into(&mut raw_frame, &true).unwrap();
        info.rust_cache_linked = RustCacheLinked::Check;
        let incremental_frame = encode_response(&Response::StorageHandshake(info.clone())).unwrap();
        info.rust_cache_linked_max_entry_bytes = 1024;
        let limited_frame = encode_response(&Response::StorageHandshake(info.clone())).unwrap();
        info.rust_hash_cargo_env = RustHashCargoEnv::Semantic;
        let semantic_frame = encode_response(&Response::StorageHandshake(info.clone())).unwrap();
        assert!(limited_frame.starts_with(&incremental_frame));
        assert!(semantic_frame.starts_with(&limited_frame));
        let Response::StorageHandshake(semantic) = decode_response(&semantic_frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert_eq!(semantic.rust_hash_cargo_env, RustHashCargoEnv::Semantic);
        assert_eq!(semantic.rust_cache_linked_max_entry_bytes, 1024);
        assert!(!semantic.put_by_path);

        info.put_by_path = true;
        let path_frame = encode_response(&Response::StorageHandshake(info.clone())).unwrap();
        assert!(path_frame.starts_with(&semantic_frame));
        // The previous decoder used bincode::deserialize for the last field.
        assert_eq!(
            bincode::deserialize::<RustHashCargoEnv>(&path_frame[limited_frame.len()..]).unwrap(),
            RustHashCargoEnv::Semantic
        );
        let Response::StorageHandshake(path_store) = decode_response(&path_frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert!(path_store.put_by_path);
        assert!(path_store.write_raw_objects);

        info.rust_cache_linked = RustCacheLinked::Off;
        info.rust_cache_linked_max_entry_bytes = default_rust_cache_linked_max_entry_bytes();
        info.rust_hash_cargo_env = RustHashCargoEnv::All;
        let frame = encode_response(&Response::StorageHandshake(info)).unwrap();
        let Response::StorageHandshake(path_store) = decode_response(&frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert!(path_store.put_by_path);
        assert_eq!(path_store.rust_cache_linked, RustCacheLinked::Off);
        assert_eq!(path_store.rust_hash_cargo_env, RustHashCargoEnv::All);
        let Response::StorageHandshake(limited) = decode_response(&limited_frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert_eq!(limited.rust_cache_linked, RustCacheLinked::Check);
        assert_eq!(limited.rust_cache_linked_max_entry_bytes, 1024);
        assert_eq!(limited.rust_hash_cargo_env, RustHashCargoEnv::All);
        assert!(incremental_frame.starts_with(&raw_frame));

        let (legacy_mode, remaining) =
            decode_frame::<crate::config::RustIncremental>(&plain_frame[old_frame.len()..])
                .unwrap();
        assert_eq!(legacy_mode, mode);
        let (legacy_explain, remaining) = decode_frame::<bool>(remaining).unwrap();
        assert!(legacy_explain);
        assert_eq!(bincode::deserialize::<u64>(remaining).unwrap(), 0);
        assert_eq!(
            bincode::deserialize::<RustIncrementalFirstBuild>(
                &incremental_frame[explain_and_prune.len()..]
            )
            .unwrap(),
            Incremental
        );

        let Response::StorageHandshake(info) = decode_response(&legacy_frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert!(!info.write_raw_objects);
        assert_eq!(info.rust_incremental.first_build, Incremental);

        let Response::StorageHandshake(info) = decode_response(&raw_frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert!(info.write_raw_objects);
        assert_eq!(info.rust_cache_linked, RustCacheLinked::Off);

        let Response::StorageHandshake(info) = decode_response(&incremental_frame).unwrap() else {
            panic!("expected storage handshake");
        };
        assert!(info.write_raw_objects);
        assert_eq!(info.rust_cache_linked, RustCacheLinked::Check);

        for (response, mode, first_build, miss_explain, prune_days) in [
            (
                bincode::deserialize(&plain_frame).unwrap(),
                Off,
                Plain,
                false,
                7,
            ),
            (
                decode_response(&mode_only).unwrap(),
                mode,
                Incremental,
                false,
                7,
            ),
            (
                decode_response(&explain_only).unwrap(),
                mode,
                Incremental,
                true,
                7,
            ),
            (
                decode_response(&explain_and_prune).unwrap(),
                mode,
                Incremental,
                true,
                0,
            ),
            (decode_response(&plain_frame).unwrap(), mode, Plain, true, 0),
            (
                decode_response(&incremental_frame).unwrap(),
                mode,
                Incremental,
                false,
                7,
            ),
        ] {
            let Response::StorageHandshake(info) = response else {
                panic!("expected storage handshake");
            };
            assert_eq!(
                info.rust_incremental,
                RustIncrementalSettings { mode, first_build }
            );
            assert_eq!(info.miss_explain, miss_explain);
            assert_eq!(info.incremental_prune_days, prune_days);
            assert_eq!(info.cache_mode, CacheMode::ReadOnly);
        }
    }

    #[test]
    fn storage_put_by_path_round_trips() {
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"/work/obj-\xff".to_vec())
        };
        #[cfg(not(unix))]
        let path = "C:\\work\\obj".into();
        let objects = vec![StoreObject {
            key: "object".into(),
            path,
            device: 13,
            inode: 37,
            size: 8192,
        }];
        let frame = encode_request(&Request::StoragePutByPath {
            key: "01".into(),
            objects: objects.clone(),
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        })
        .unwrap();
        let Request::StoragePutByPath {
            key,
            objects: decoded,
            stdout,
            stderr,
        } = decode_request(&frame).unwrap()
        else {
            panic!("expected path store");
        };
        assert_eq!(key, "01");
        assert_eq!(decoded, objects);
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
    }
}
