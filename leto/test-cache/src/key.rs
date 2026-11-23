use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{
    observer,
    store::{self, Stamp},
};

pub const LOADER: &str = "/lib64/ld-linux-x86-64.so.2";
const LOADER_CONFIG: &[&str] = &["/etc/ld.so.cache", "/etc/ld.so.preload"];
const DEFAULT_ENV: &str = "LANG,LC_*,TZ,RUST_*,PROPTEST_*";
const PATH_ENV: [&str; 2] = ["CARGO_MANIFEST_DIR", "NEXTEST_WORKSPACE_ROOT"];

pub struct Identity {
    pub key: blake3::Hash,
    pub binary_digest: blake3::Hash,
    pub mtime: (i64, i64),
    pub libraries: Vec<PathBuf>,
}

pub struct StageOne {
    pub environment: BTreeMap<OsString, OsString>,
    pub output_key: [u8; 32],
    pub stamps: Vec<(PathBuf, Stamp)>,
}

impl StageOne {
    pub fn unchanged(&self) -> bool {
        self.stamps
            .iter()
            .all(|(path, saved)| Stamp::read(path).is_ok_and(|stamp| stamp == *saved))
    }
}

fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn normalized(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            _ => result.push(part),
        }
    }
    result
}

fn based_path(value: &OsStr, bases: &[PathBuf]) -> OsString {
    let path = Path::new(value);
    if !path.is_absolute() {
        return value.to_owned();
    }
    let path = normalized(path);
    let Some(relative) = bases
        .iter()
        .filter_map(|base| {
            path.strip_prefix(base)
                .ok()
                .map(|relative| (base.as_os_str().len(), relative))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, relative)| relative)
    else {
        return value.to_owned();
    };
    Path::new(".").join(relative).into_os_string()
}

fn filtered_env(
    env: impl IntoIterator<Item = (OsString, OsString)>,
    configured: &str,
    bases: &[PathBuf],
) -> (BTreeMap<OsString, OsString>, usize) {
    let rules: Vec<_> = DEFAULT_ENV
        .split(',')
        .chain(configured.split(','))
        .filter(|rule| !rule.is_empty())
        .collect();
    let mut env_key = BTreeMap::new();
    let mut manifest_len = 0;
    for (name, mut value) in env {
        let bytes = name.as_bytes();
        if PATH_ENV.iter().any(|key| name == *key) {
            if name == PATH_ENV[0] {
                manifest_len = value.len();
            }
            value = based_path(&value, bases);
        } else if bytes == b"TMPDIR"
            || bytes.starts_with(b"NEXTEST_")
            || bytes == b"NEXTEST"
            || bytes.starts_with(b"LETO_TEST_CACHE")
            || !(bytes.starts_with(b"CARGO_PKG_")
                || rules.iter().any(|rule| {
                    rule.strip_suffix('*')
                        .map_or(bytes == rule.as_bytes(), |prefix| {
                            bytes.starts_with(prefix.as_bytes())
                        })
                }))
        {
            continue;
        }
        env_key.insert(name, value);
    }
    (env_key, manifest_len)
}

pub fn build(
    root: &Path,
    binary: &Path,
    args: &[OsString],
    checkout: &Path,
    now: Duration,
) -> io::Result<Identity> {
    build_inner(root, binary, args, checkout, now, None).map(|(identity, _)| identity)
}

pub fn stage_one(
    root: &Path,
    binary: &Path,
    args: &[OsString],
    checkout: &Path,
    now: Duration,
    write_memo: bool,
) -> io::Result<(Identity, StageOne)> {
    let (mut identity, stage) = build_inner(root, binary, args, checkout, now, Some(write_memo))?;
    identity.key = crate::sandbox::key(identity.key);
    Ok((identity, stage.unwrap()))
}

fn build_inner(
    root: &Path,
    input: &Path,
    args: &[OsString],
    checkout: &Path,
    now: Duration,
    stage_one: Option<bool>,
) -> io::Result<(Identity, Option<StageOne>)> {
    let cwd = std::env::current_dir()?;
    let binary = fs::canonicalize(cwd.join(input))?;
    let stamp = Stamp::read(&binary)?;
    let write_memo = stage_one.unwrap_or(true);
    let binary_digest = store::digest(root, &binary, stamp, now, write_memo)?;
    let loader_env = loader_environment(std::env::vars_os());
    let libraries = closure(root, &binary, binary_digest, &cwd, &loader_env, write_memo)?;
    let configured = std::env::var("LETO_TEST_CACHE_ENV").unwrap_or_default();
    let mut bases = vec![normalized(checkout)];
    if let Some(value) = std::env::var_os("SCCACHE_BASEDIRS") {
        bases.extend(
            value
                .as_bytes()
                .split(|&b| b == b':')
                .map(OsStr::from_bytes)
                .map(Path::new)
                .filter(|p| p.is_absolute())
                .map(normalized),
        );
    }
    let environment = filtered_env(std::env::vars_os(), &configured, &bases);
    let mut profile = blake3::Hasher::new();
    field(&mut profile, observer::PROFILE.as_bytes());
    for instruction in observer::filter() {
        profile.update(&instruction.code.to_le_bytes());
        profile.update(&[instruction.jt, instruction.jf]);
        profile.update(&instruction.k.to_le_bytes());
    }
    field(&mut profile, DEFAULT_ENV.as_bytes());
    field(&mut profile, configured.as_bytes());
    let mut linked = Vec::with_capacity(libraries.len());
    let mut stamps = Vec::new();
    for path in &libraries {
        let stamp = Stamp::read(path)?;
        linked.push((path, store::digest(root, path, stamp, now, write_memo)?));
        if stage_one.is_some() {
            stamps.push((path.clone(), stamp));
        }
    }

    // SAFETY: uname initializes the utsname on success and its fields are NUL-terminated.
    let mut host: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut host) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let kernel = unsafe { CStr::from_ptr(host.release.as_ptr()) }.to_bytes();
    let key = compose(
        profile.finalize(),
        binary_digest,
        &linked,
        args,
        &environment,
        kernel,
    );
    if Stamp::read(&binary)? != stamp {
        return Err(io::Error::other("test binary changed while computing key"));
    }
    let stage = if stage_one.is_some() {
        let execution = filtered_env(std::env::vars_os(), &configured, &[]).0;
        let effective_loader_env = loader_environment(execution.clone());
        if effective_loader_env != loader_env
            && closure(
                root,
                &binary,
                binary_digest,
                &cwd,
                &effective_loader_env,
                write_memo,
            )? != libraries
        {
            return Err(io::Error::other(
                "filtered environment changes the library closure",
            ));
        }
        if execution.keys().any(|name| {
            name.as_bytes().starts_with(b"LD_") && !effective_loader_env.contains_key(name)
        }) {
            return Err(io::Error::other("unsupported keyed dynamic-loader setting"));
        }
        stamps.push((cwd.join(input), stamp));
        let mut output_key = blake3::Hasher::new_derive_key("leto test cache captured output v1");
        for (name, value) in &environment.0 {
            field(&mut output_key, name.as_bytes());
            field(&mut output_key, value.as_bytes());
        }
        field(&mut output_key, key.as_bytes());
        Some(StageOne {
            environment: execution,
            output_key: *output_key.finalize().as_bytes(),
            stamps,
        })
    } else {
        None
    };
    Ok((
        Identity {
            key,
            binary_digest,
            mtime: stamp.mtime,
            libraries,
        },
        stage,
    ))
}

fn compose(
    profile: blake3::Hash,
    binary: blake3::Hash,
    libraries: &[(&PathBuf, blake3::Hash)],
    args: &[OsString],
    env: &(BTreeMap<OsString, OsString>, usize),
    kernel: &[u8],
) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    field(&mut hash, b"leto-test-key-v1");
    field(&mut hash, profile.as_bytes());
    field(&mut hash, binary.as_bytes());
    field(&mut hash, &(libraries.len() as u64).to_le_bytes());
    for (path, digest) in libraries {
        field(&mut hash, path.as_os_str().as_bytes());
        field(&mut hash, digest.as_bytes());
    }
    field(&mut hash, &(args.len() as u64).to_le_bytes());
    for arg in args {
        field(&mut hash, arg.as_bytes());
    }
    field(&mut hash, &(env.0.len() as u64).to_le_bytes());
    for (name, value) in &env.0 {
        field(&mut hash, name.as_bytes());
        field(&mut hash, value.as_bytes());
    }
    field(&mut hash, &(env.1 as u64).to_le_bytes());
    field(&mut hash, kernel);
    field(&mut hash, std::env::consts::ARCH.as_bytes());
    hash.finalize()
}

fn loader(binary: &Path) -> io::Result<bool> {
    let unsupported = || io::Error::other("unsupported test harness ELF or loader");
    let file = File::open(binary)?;
    let metadata = file.metadata()?;
    if metadata.mode() & 0o6000 != 0 {
        return Err(unsupported());
    }
    let mut header = [0; 64];
    file.read_exact_at(&mut header, 0)?;
    if &header[..6] != b"\x7fELF\x02\x01"
        || u16_at(&header, 18) != 62
        || u16_at(&header, 54) != 56
        || u16_at(&header, 56) > 256
    {
        return Err(unsupported());
    }
    let mut dynamic = false;
    for index in 0..u64::from(u16_at(&header, 56)) {
        let mut program = [0; 56];
        let offset = u64_at(&header, 32)
            .checked_add(index * 56)
            .ok_or_else(unsupported)?;
        file.read_exact_at(&mut program, offset)?;
        let offset = u64_at(&program, 8);
        let size = u64_at(&program, 32);
        match u32::from_le_bytes(program[..4].try_into().unwrap()) {
            3 => {
                let mut interp = [0; 128];
                let length = usize::try_from(size)
                    .ok()
                    .filter(|&s| s <= interp.len())
                    .ok_or_else(unsupported)?;
                file.read_exact_at(&mut interp[..length], offset)?;
                if interp[..length].strip_suffix(b"\0") != Some(LOADER.as_bytes()) {
                    return Err(unsupported());
                }
                dynamic = true;
            }
            2 => {
                if size > 65536 {
                    return Err(unsupported());
                }
                let mut entries = vec![0; size as usize];
                file.read_exact_at(&mut entries, offset)?;
                for entry in entries.chunks_exact(16) {
                    match u64_at(entry, 0) {
                        0 => break,
                        0x6fff_fefb | 0x6fff_fefc => return Err(unsupported()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(dynamic)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn config_stamp(path: &Path) -> io::Result<Option<String>> {
    match Stamp::read(path) {
        Ok(stamp) => Ok(Some(stamp.id())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn loader_environment(
    env: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    env.into_iter()
        .filter(|(name, _)| {
            matches!(
                name.to_str(),
                Some("LD_LIBRARY_PATH" | "LD_HWCAP_MASK" | "LD_ORIGIN_PATH" | "GLIBC_TUNABLES")
            )
        })
        .collect()
}

fn closure(
    root: &Path,
    binary: &Path,
    digest: blake3::Hash,
    cwd: &Path,
    loader_env: &BTreeMap<OsString, OsString>,
    write_memo: bool,
) -> io::Result<Vec<PathBuf>> {
    // Let the system loader resolve transitive RUNPATH, hwcaps and the cache in its normal order.
    if ["LD_AUDIT", "LD_PRELOAD"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
        || fs::metadata("/etc/ld.so.preload").is_ok_and(|m| m.len() != 0)
    {
        return Err(io::Error::other(
            "cannot shadow with loader audit or preload",
        ));
    }
    let mut context = blake3::Hasher::new();
    field(&mut context, b"loader-list-v1");
    field(&mut context, digest.as_bytes());
    field(&mut context, binary.as_os_str().as_bytes());
    field(&mut context, cwd.as_os_str().as_bytes());
    for (name, value) in loader_env {
        field(&mut context, name.as_bytes());
        field(&mut context, value.as_bytes());
    }
    for path in LOADER_CONFIG.iter().copied().chain([LOADER]) {
        field(
            &mut context,
            config_stamp(Path::new(path))?
                .unwrap_or_default()
                .as_bytes(),
        );
    }
    let memo = root
        .join("closures")
        .join(context.finalize().to_hex().as_str());
    if let Ok(bytes) = fs::read(&memo)
        && let Ok((libraries, watches)) =
            serde_json::from_slice::<(Vec<Vec<u8>>, Vec<(Vec<u8>, String)>)>(&bytes)
        && watches.iter().all(|(path, stamp)| {
            Stamp::read(Path::new(OsStr::from_bytes(path))).is_ok_and(|seen| seen.id() == *stamp)
        })
    {
        return Ok(libraries
            .into_iter()
            .map(|path| OsString::from_vec(path).into())
            .collect());
    }

    if !loader(binary)? {
        return Ok(Vec::new());
    }
    let meta = fs::metadata(LOADER)?;
    if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err(io::Error::other(
            "system dynamic loader is writable or untrusted",
        ));
    }
    let output = Command::new(LOADER)
        .arg("--list")
        .arg(binary)
        .env_clear()
        .envs(loader_env)
        .env("LD_DEBUG", "libs")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "system loader could not resolve the test harness",
        ));
    }
    let mut objects = BTreeSet::from([PathBuf::from(LOADER)]);
    let mut watched = BTreeSet::new();
    for line in output.stdout.split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() || line.starts_with(b"linux-vdso.so.") {
            continue;
        }
        let end = line
            .windows(4)
            .rposition(|w| w == b" (0x")
            .ok_or_else(|| io::Error::other("unresolved or unrecognized shared object"))?;
        let name = &line[..end];
        let name = name
            .windows(4)
            .position(|w| w == b" => ")
            .map_or(name, |i| &name[i + 4..]);
        let path = cwd.join(OsStr::from_bytes(name));
        objects.insert(path);
    }
    for line in output.stderr.split(|&b| b == b'\n') {
        if let Some(i) = line.windows(12).position(|w| w == b"trying file=") {
            let candidate = cwd.join(OsStr::from_bytes(line[i + 12..].trim_ascii_end()));
            if let Some(dir) = candidate
                .parent()
                .and_then(|dir| dir.ancestors().find(|p| p.is_dir()))
            {
                watched.insert(dir.to_path_buf());
            }
        }
    }
    watched.extend(objects.iter().cloned());
    let stamp = watched
        .iter()
        .map(|path| Ok((path.as_os_str().as_bytes(), Stamp::read(path)?.id())))
        .collect::<io::Result<Vec<_>>>()?;
    let libraries: Vec<_> = objects
        .iter()
        .map(|path| path.as_os_str().as_bytes())
        .collect();
    if write_memo {
        store::replace(&memo, &serde_json::to_vec(&(libraries, stamp))?)?;
    }
    Ok(objects.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn environment(base: &str, noise: &str) -> (BTreeMap<OsString, OsString>, usize) {
        let env = json!({
            "CARGO_MANIFEST_DIR": format!("{base}/crate"), "NEXTEST_WORKSPACE_ROOT": base,
            "CARGO_PKG_NAME": "toy", "NEXTEST_RUN_ID": noise, "NEXTEST_TEST_GLOBAL_SLOT": noise,
            "TMPDIR": noise, "FAIRFILL_FOO": "setting", "UNLISTED": noise,
        });
        filtered_env(
            env.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.into(), v.as_str().unwrap().into())),
            "FAIRFILL_*,TMPDIR,NEXTEST_*",
            &[base.into()],
        )
    }

    #[test]
    fn keys_frame_arguments_and_include_only_selected_environment() {
        let en = environment("/first", "a");
        assert_eq!(en, environment("/other", "b"));
        assert_eq!(en.0.len(), 4);
        assert_eq!(en.0[OsStr::new("CARGO_MANIFEST_DIR")], "./crate");
        assert_eq!(en.0[OsStr::new("NEXTEST_WORKSPACE_ROOT")], "./");
        assert_ne!(en, environment("/longer", "a"));
        assert_eq!(
            based_path(
                OsStr::new("/base/sub/./file"),
                &["/base".into(), "/base/sub".into()]
            ),
            "./file"
        );
        assert_eq!(
            based_path(OsStr::new("/base-other/file"), &["/base".into()]),
            "/base-other/file"
        );

        let profile = blake3::hash(b"profile");
        let binary = blake3::hash(b"binary");
        let path = PathBuf::from("/lib/libc.so");
        let libraries = [(&path, blake3::hash(b"library"))];
        let args = [OsString::from_vec(vec![b'a', 0xff]), "bc".into()];
        let key = compose(profile, binary, &libraries, &args, &en, b"kernel");
        assert_ne!(
            key,
            compose(
                profile,
                binary,
                &libraries,
                &["a".into(), OsString::from_vec(vec![0xff, b'b', b'c'])],
                &en,
                b"kernel"
            )
        );
        assert_ne!(
            key,
            compose(profile, binary, &libraries, &args, &en, b"other")
        );
        assert_ne!(key, compose(profile, binary, &[], &args, &en, b"kernel"));
        assert_ne!(
            key,
            compose(binary, binary, &libraries, &args, &en, b"kernel")
        );
        assert_ne!(
            key,
            compose(profile, profile, &libraries, &args, &en, b"kernel")
        );
        let mut changed = en.clone();
        changed.0.insert("FAIRFILL_FOO".into(), "new".into());
        assert_ne!(
            key,
            compose(profile, binary, &libraries, &args, &changed, b"kernel")
        );
    }
}
