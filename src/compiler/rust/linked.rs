use super::{ArgCodegen, ArgData, ArgLinkPath, ArgTarget, ArgUnstable, Argument};
use crate::cache::CacheRead;
use crate::errors::*;
use crate::mock_command::{CommandCreatorSync, RunCommand};
use crate::util::{Digest, HashToDigest, run_input_output};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::hash::Hash;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};
use tempfile::{NamedTempFile, TempDir};

pub(crate) const MEMBER: &str = ".sccache-link-inputs";
const MAX_MANIFEST_LEN: usize = 8 * 1024 * 1024;
const LINK_ENV: &[&str] = &[
    "LIBRARY_PATH",
    "LD_LIBRARY_PATH",
    "LD_RUN_PATH",
    "GCC_EXEC_PREFIX",
    "COMPILER_PATH",
    "SOURCE_DATE_EPOCH",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "CCC_OVERRIDE_OPTIONS",
    "LDEMULATION",
];

fn linker_env(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    LINK_ENV.contains(&name) || name.starts_with("MOLD_") || name.starts_with("LLD_")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Options {
    driver: Option<String>,
    flavor: Option<String>,
    target: Option<String>,
    fuse_ld: Option<String>,
    search: Vec<(PathBuf, bool)>,
}

impl Options {
    pub(super) fn parse(
        arguments: &[Argument<ArgData>],
    ) -> std::result::Result<Self, &'static str> {
        let mut options = Self::default();
        let mut link_args = Vec::new();
        for argument in arguments {
            match argument.get_data() {
                Some(ArgData::CodeGen(ArgCodegen { opt, value })) => {
                    let value = value.as_deref();
                    let enabled = !matches!(value, Some("off" | "false" | "no" | "n" | "0"));
                    match opt.replace('_', "-").as_str() {
                        "linker" => options.driver = value.map(str::to_owned),
                        "linker-flavor" => options.flavor = value.map(str::to_owned),
                        "link-arg" => link_args.extend(value),
                        "link-args" => {
                            link_args.extend(value.unwrap_or_default().split_whitespace());
                        }
                        "rpath" if enabled => return Err("rpath"),
                        "linker-plugin-lto" if enabled => return Err("linker-plugin-lto"),
                        "lto" if enabled => return Err("lto"),
                        "instrument-coverage" if enabled => return Err("instrument-coverage"),
                        "profile-generate" => return Err("profile-generate"),
                        "profile-use" => return Err("profile-use"),
                        "link-dead-code" if enabled => return Err("link-dead-code"),
                        "split-debuginfo" if value != Some("off") => return Err("split-debuginfo"),
                        "save-temps" if enabled => return Err("save-temps"),
                        _ => {}
                    }
                }
                Some(ArgData::Unstable(ArgUnstable { opt, .. })) => {
                    match opt.replace('_', "-").as_str() {
                        "sanitizer" => return Err("sanitizer"),
                        "profile" | "coverage-options" | "no-link" | "link-only" => {
                            return Err("unsupported linked codegen");
                        }
                        _ => {}
                    }
                }
                Some(ArgData::Target(ArgTarget::Name(target))) => {
                    options.target = Some(target.clone());
                }
                Some(ArgData::Target(_)) => return Err("linked target"),
                Some(ArgData::LinkPath(ArgLinkPath { kind, path })) => match kind.as_str() {
                    "all" | "native" => options.search.push((path.clone(), true)),
                    "dependency" | "crate" => options.search.push((path.clone(), false)),
                    _ => return Err("linked library search kind"),
                },
                _ => {}
            }
        }
        let link_args = link_args.iter().flat_map(|arg| {
            arg.strip_prefix("-Wl,").map_or_else(
                || itertools::Either::Left(std::iter::once(*arg)),
                |arg| itertools::Either::Right(arg.split(',')),
            )
        });
        let mut link_args = link_args.peekable();
        while let Some(arg) = link_args.next() {
            if let Some(linker) = arg.strip_prefix("-fuse-ld=") {
                if !matches!(linker, "bfd" | "lld" | "mold") {
                    return Err("linker flavor");
                }
                options.fuse_ld = Some(linker.to_owned());
            } else if let Some(dir) = arg
                .strip_prefix("-L")
                .or_else(|| arg.strip_prefix("--library-path="))
            {
                let dir = if dir.is_empty() {
                    link_args.next().ok_or("linked library search path")?
                } else {
                    dir
                };
                options.search.push((PathBuf::from(dir), true));
            } else if arg.starts_with('@')
                || arg.starts_with("--dependency-file")
                || arg.starts_with("-rpath")
                || arg.starts_with("--rpath")
                || arg.starts_with("-Map")
                || arg.starts_with("--Map")
                || arg.starts_with("--out-implib")
                || arg.starts_with("--sysroot")
                || arg.starts_with("--ld-path")
                || (arg.starts_with("-B") && !matches!(arg, "-Bstatic" | "-Bdynamic"))
                || matches!(arg, "-Xlinker" | "-fuse-ld" | "--library-path")
            {
                return Err("unsupported linker argument");
            }
        }
        options.uses_driver()?;
        Ok(options)
    }

    pub(super) fn uses_driver(&self) -> std::result::Result<bool, &'static str> {
        match self.flavor.as_deref() {
            None => {
                let name = self
                    .driver
                    .as_deref()
                    .and_then(|driver| Path::new(driver).file_name());
                let direct = name.is_some_and(|name| {
                    matches!(name.to_str(), Some("ld" | "ld.lld" | "rust-lld"))
                });
                if direct && self.fuse_ld.is_some() {
                    return Err("linker flavor");
                }
                Ok(!direct)
            }
            Some("gcc" | "gnu-cc" | "gnu-lld-cc") => Ok(true),
            Some("ld" | "gnu" | "gnu-lld") if self.fuse_ld.is_none() => Ok(false),
            _ => Err("linker flavor"),
        }
    }

    pub(super) async fn prepare<T: CommandCreatorSync>(
        &self,
        creator: &T,
        host: &str,
        sysroot: &Path,
        cwd: &Path,
        env: &[(OsString, OsString)],
        basedirs: &[Vec<u8>],
    ) -> Result<(Identity, SearchPaths)> {
        ensure!(
            cfg!(target_os = "linux")
                && host.ends_with("-linux-gnu")
                && self.target.as_deref().is_none_or(|target| target == host),
            "unsupported linked target"
        );
        let get_env = |key| {
            env.iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, v)| v)
        };
        ensure!(
            ["LD_PRELOAD", "LD_AUDIT", "CCC_OVERRIDE_OPTIONS"]
                .into_iter()
                .all(|key| get_env(key).is_none_or(|value| value.is_empty())),
            "environment overrides linker invocation"
        );
        let bin = sysroot.join("lib/rustlib").join(host).join("bin");
        let mut path = vec![bin.clone(), bin.join("self-contained")];
        path.extend(std::env::split_paths(
            get_env("PATH").context("PATH is unset")?,
        ));
        let path = std::env::join_paths(path)?;
        let uses_driver = self.uses_driver().map_err(|reason| anyhow!(reason))?;
        let lld = bin.join("rust-lld");
        let driver_name = self.driver.as_deref().unwrap_or(if uses_driver {
            "cc"
        } else if self.flavor.as_deref() == Some("gnu-lld") {
            lld.to_str().context("non-UTF-8 rust-lld path")?
        } else {
            "ld"
        });
        let locate = |name: &OsStr| which::which_in(name, Some(&path), cwd);
        let driver = locate(OsStr::new(driver_name))?;
        let direct_args =
            if !uses_driver && Path::new(driver_name).file_name() == Some(OsStr::new("rust-lld")) {
                vec!["-flavor".into(), "gnu".into()]
            } else {
                vec![]
            };
        let (driver_identity, driver_stamp) =
            tool_identity(creator, &driver, &direct_args, cwd, env, &path).await?;
        let mut tools = vec![(driver.clone(), driver_stamp)];
        let mut linkers = Vec::new();
        let mut search = self.search.clone();

        if uses_driver {
            let mut args = Vec::new();
            if let Some(fuse_ld) = &self.fuse_ld {
                args.push(format!("-fuse-ld={fuse_ld}"));
            }
            let name = self
                .fuse_ld
                .as_ref()
                .map_or_else(|| "ld".to_owned(), |flavor| format!("ld.{flavor}"));
            let mut locate_args = args.clone();
            locate_args.push(format!("-print-prog-name={name}"));
            let program = run_tool(creator, &driver, &locate_args, cwd, env, &path).await?;
            let program = std::str::from_utf8(&program.stdout)?.trim();
            if let Ok(linker) = locate(OsStr::new(program)) {
                linkers.push((linker, vec![]));
            }

            // rustc can select its bundled linker before the driver's default.
            if matches!(self.fuse_ld.as_deref(), None | Some("lld")) && lld.is_file() {
                let shim = bin.join("gcc-ld/ld.lld");
                if shim.is_file() {
                    linkers.push((shim, vec![]));
                }
                linkers.push((lld, vec!["-flavor".to_owned(), "gnu".to_owned()]));
            }

            args.push("-print-search-dirs".to_owned());
            let dirs = run_tool(creator, &driver, &args, cwd, env, &path).await?;
            let libraries = std::str::from_utf8(&dirs.stdout)?
                .lines()
                .find_map(|line| line.strip_prefix("libraries: ="))
                .context("linker driver did not report its search directories")?;
            search.extend(std::env::split_paths(libraries).map(|dir| (dir, false)));
        } else {
            linkers.push((driver.clone(), direct_args));
        }
        for key in ["LIBRARY_PATH", "LD_LIBRARY_PATH"] {
            if let Some(value) = get_env(key) {
                search.extend(std::env::split_paths(value).map(|dir| {
                    let local = matches!(
                        LinkPath::new(&cwd.join(&dir), basedirs, Some(sysroot)),
                        LinkPath::Basedir(..)
                    );
                    (dir, key == "LD_LIBRARY_PATH" && local)
                }));
            }
        }
        let mut supported = false;
        let mut linker_identities = Vec::new();
        for (linker, args) in linkers {
            let (identity, stamp) = tool_identity(creator, &linker, &args, cwd, env, &path).await?;
            tools.push((linker, stamp));
            supported |= identity.gnu_depfile;
            linker_identities.push(identity);
        }
        ensure!(supported, "linker does not support GNU dependency files");
        let identity = Identity {
            driver: driver_identity,
            linkers: linker_identities,
            uses_driver,
        };
        let search = SearchPaths {
            directories: search
                .into_iter()
                .map(|(p, native)| (cwd.join(p), native))
                .collect(),
            tools,
        };
        Ok((identity, search))
    }
}

#[derive(Clone, Debug, Hash)]
struct ToolIdentity {
    digest: String,
    version: (Vec<u8>, Vec<u8>),
    gnu_depfile: bool,
}

#[derive(Clone, Debug, Hash)]
pub(super) struct Identity {
    driver: ToolIdentity,
    linkers: Vec<ToolIdentity>,
    uses_driver: bool,
}

impl Identity {
    pub(super) fn hash_key(
        &self,
        digest: &mut Digest,
        env: &[(OsString, OsString)],
        basedirs: &[Vec<u8>],
    ) {
        digest.delimiter(b"rust-linked-v1-proc-macro");
        let mut env: Vec<_> = env.iter().filter(|(key, _)| linker_env(key)).collect();
        env.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        let mut hash = HashToDigest { digest };
        self.hash(&mut hash);
        env.len().hash(&mut hash);
        for (key, value) in env {
            key.hash(&mut hash);
            if key == "LD_LIBRARY_PATH" {
                // Cargo adds its checkout's deps directory to the loader path.
                let paths: Vec<_> = std::env::split_paths(value)
                    .map(|path| LinkPath::new(&path, basedirs, None))
                    .collect();
                paths.hash(&mut hash);
            } else {
                value.hash(&mut hash);
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    device: u64,
    inode: u64,
}

impl Stamp {
    fn from_metadata(metadata: &Metadata) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                size: metadata.len(),
                mtime: (metadata.mtime(), metadata.mtime_nsec()),
                ctime: (metadata.ctime(), metadata.ctime_nsec()),
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            bail!("unsupported linked filesystem")
        }
    }

    fn of(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        ensure!(metadata.is_file(), "not a regular link input: {path:?}");
        Self::from_metadata(&metadata)
    }

    fn changed_since(&self, since: SystemTime) -> bool {
        let Ok(since) = since.duration_since(SystemTime::UNIX_EPOCH) else {
            return true;
        };
        let since = (since.as_secs() as i64, i64::from(since.subsec_nanos()));
        self.ctime >= since || self.mtime >= since
    }
}

static FILE_DIGESTS: LazyLock<Mutex<HashMap<Stamp, String>>> = LazyLock::new(Default::default);

fn file_digest(path: &Path, stamp: &Stamp) -> Result<String> {
    let memoize = !stamp.changed_since(SystemTime::now() - Duration::from_secs(2));
    if memoize && let Some(digest) = FILE_DIGESTS.lock().unwrap().get(stamp).cloned() {
        ensure!(Stamp::of(path)? == *stamp, "link input changed: {path:?}");
        return Ok(digest);
    }
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "not a regular link input: {path:?}");
    ensure!(
        Stamp::from_metadata(&metadata)? == *stamp,
        "link input changed: {path:?}"
    );
    let mut reader = BufReader::new(&file);
    ensure!(
        !reader.fill_buf()?.starts_with(b"!<thin>\n"),
        "thin archive members may be absent from the link depfile: {path:?}"
    );
    let digest = Digest::reader_sync(&mut reader)?;
    ensure!(
        Stamp::from_metadata(&file.metadata()?)? == *stamp && Stamp::of(path)? == *stamp,
        "link input changed: {path:?}"
    );
    if memoize {
        let mut memo = FILE_DIGESTS.lock().unwrap();
        if memo.len() >= 8192 {
            memo.clear();
        }
        memo.insert(stamp.clone(), digest.clone());
    }
    Ok(digest)
}

async fn run_tool<T: CommandCreatorSync>(
    creator: &T,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(OsString, OsString)],
    path: &OsStr,
) -> Result<std::process::Output> {
    let mut creator = creator.clone();
    let mut command = creator.new_command_sync(tool);
    command
        .env_clear()
        .envs(env.to_vec())
        .env("PATH", path)
        .env("LC_ALL", "C")
        .current_dir(cwd)
        .args(args);
    run_input_output(command, None).await
}

type ToolKey = (PathBuf, Stamp, Vec<String>, Vec<(OsString, OsString)>);
static TOOL_IDENTITIES: LazyLock<Mutex<HashMap<ToolKey, ToolIdentity>>> =
    LazyLock::new(Default::default);

async fn tool_identity<T: CommandCreatorSync>(
    creator: &T,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(OsString, OsString)],
    path: &OsStr,
) -> Result<(ToolIdentity, Stamp)> {
    let stamp = Stamp::of(tool)?;
    let memoize = !stamp.changed_since(SystemTime::now() - Duration::from_secs(2));
    let mut tool_env: Vec<_> = env
        .iter()
        .filter(|(key, _)| linker_env(key))
        .cloned()
        .collect();
    tool_env.extend([
        ("PATH".into(), path.to_owned()),
        ("cwd".into(), cwd.as_os_str().to_owned()),
    ]);
    tool_env.sort_unstable();
    let key = (tool.to_owned(), stamp.clone(), args.to_owned(), tool_env);
    if memoize && let Some(identity) = TOOL_IDENTITIES.lock().unwrap().get(&key).cloned() {
        return Ok((identity, stamp));
    }
    let mut args = args.to_vec();
    args.push("--version".to_owned());
    let version = run_tool(creator, tool, &args, cwd, env, path).await?;
    let gnu = [b"GNU ld".as_slice(), b"LLD ", b"Ubuntu LLD ", b"mold "]
        .iter()
        .any(|prefix| version.stdout.starts_with(prefix));
    let gnu_depfile = if gnu {
        *args.last_mut().unwrap() = "--help".to_owned();
        let help = run_tool(creator, tool, &args, cwd, env, path).await?;
        help.stdout
            .windows(b"--dependency-file".len())
            .any(|part| part == b"--dependency-file")
    } else {
        false
    };
    let identity = ToolIdentity {
        digest: file_digest(tool, &stamp)?,
        version: (version.stdout, version.stderr),
        gnu_depfile,
    };
    if memoize {
        let mut memo = TOOL_IDENTITIES.lock().unwrap();
        if memo.len() >= 128 {
            memo.clear();
        }
        memo.insert(key, identity.clone());
    }
    Ok((identity, stamp))
}

#[derive(Clone, Debug)]
pub(super) struct SearchPaths {
    directories: Vec<(PathBuf, bool)>,
    tools: Vec<(PathBuf, Stamp)>,
}

impl SearchPaths {
    pub(super) fn hash_key(&self, digest: &mut Digest, basedirs: &[Vec<u8>], sysroot: &Path) {
        let mut hash = HashToDigest { digest };
        self.directories.len().hash(&mut hash);
        for (path, native) in &self.directories {
            (LinkPath::new(path, basedirs, Some(sysroot)), native).hash(&mut hash);
        }
    }

    fn verify_tools(&self) -> Result<()> {
        for (path, stamp) in &self.tools {
            ensure!(Stamp::of(path)? == *stamp, "linker changed: {path:?}");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct LinkedCompilation {
    pub(super) available: Option<SearchPaths>,
    pub(super) uses_driver: bool,
    pub(super) output: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) sysroot: PathBuf,
    pub(super) basedirs: Vec<Vec<u8>>,
}

impl LinkedCompilation {
    pub(crate) fn available(&self) -> bool {
        self.available.is_some()
    }

    pub(super) fn dependency_argument(&self, path: &Path) -> Option<String> {
        let path = path.to_str()?;
        if self.uses_driver && path.contains(',') {
            return None;
        }
        let prefix = if self.uses_driver { "-Wl," } else { "" };
        Some(format!("{prefix}--dependency-file={path}"))
    }

    pub(crate) fn capture(&self) -> Result<Capture> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("link.d");
        Ok(Capture {
            _dir: dir,
            path,
            start: SystemTime::now(),
        })
    }

    pub(crate) fn verify(&self, entry: &mut CacheRead) -> Result<()> {
        let mut bytes = LimitedManifest(Vec::new());
        entry.get_object(MEMBER, &mut bytes)?;
        let manifest: Manifest = serde_json::from_slice(&bytes.0)?;
        ensure!(!manifest.files.is_empty(), "empty link manifest");
        for input in &manifest.files {
            let path = input.path.resolve(self)?;
            let stamp = Stamp::of(&path)?;
            ensure!(
                stamp.size == input.size && file_digest(&path, &stamp)? == input.digest,
                "link input changed: {path:?}"
            );
        }
        for search in &manifest.searches {
            let path = search.path.resolve(self)?;
            let modified = match &search.path {
                LinkPath::Absolute(_) => directory_mtime(&path)?,
                _ => None,
            };
            ensure!(
                modified == search.modified
                    && search_snapshot(&path, &manifest.names, search.native, &self.output)?
                        == search.entries,
                "link search directory changed: {path:?}"
            );
        }
        self.available
            .as_ref()
            .context("missing linker identity")?
            .verify_tools()
    }

    pub(crate) fn finish_capture(&self, capture: &Capture) -> Result<NamedTempFile> {
        let mut files = String::new();
        use std::io::Read as _;
        File::open(&capture.path)?
            .take(MAX_MANIFEST_LEN as u64 + 1)
            .read_to_string(&mut files)?;
        ensure!(files.len() <= MAX_MANIFEST_LEN, "oversized linker depfile");
        let mut paths = parse_depfile(&files, &self.output, &self.cwd)?;
        paths.retain(|path| !self.is_temporary(path));
        ensure!(!paths.is_empty(), "link depfile has no persistent inputs");
        let search_paths = self.available.as_ref().context("missing linker identity")?;
        search_paths.verify_tools()?;

        // rustc can bundle a native archive before invoking the linker.
        let mut dirs = BTreeMap::new();
        for (path, native) in &search_paths.directories {
            *dirs.entry(path.to_owned()).or_default() |= *native;
        }
        for dir in paths.iter().filter_map(|path| path.parent()) {
            dirs.entry(dir.to_owned()).or_insert(false);
        }
        let names = shadow_names(&paths);
        let mut searches = Vec::new();
        for (dir, native) in dirs {
            let entries = search_snapshot(&dir, &names, native, &self.output)?;
            let path = LinkPath::new(&dir, &self.basedirs, Some(&self.sysroot));
            let modified = match &path {
                LinkPath::Absolute(_) => directory_mtime(&dir)?,
                _ => None,
            };
            paths.extend(entries.keys().map(|name| dir.join(name)));
            searches.push(Search {
                path,
                native,
                modified,
                entries,
            });
        }
        let files = paths
            .into_iter()
            .map(|path| -> Result<_> {
                let stamp = Stamp::of(&path)?;
                ensure!(
                    !stamp.changed_since(capture.start),
                    "link input changed while compiling: {path:?}"
                );
                for ancestor in path.ancestors() {
                    let metadata = fs::symlink_metadata(ancestor)?;
                    ensure!(
                        !metadata.is_symlink()
                            || !Stamp::from_metadata(&metadata)?.changed_since(capture.start),
                        "link input symlink changed while compiling: {ancestor:?}"
                    );
                }
                Ok(Input {
                    path: LinkPath::new(&path, &self.basedirs, Some(&self.sysroot)),
                    size: stamp.size,
                    mtime: stamp.mtime,
                    digest: file_digest(&path, &stamp)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut manifest = NamedTempFile::new()?;
        let mut bytes = LimitedManifest(Vec::new());
        serde_json::to_writer(
            &mut bytes,
            &Manifest {
                files,
                names,
                searches,
            },
        )?;
        manifest.write_all(&bytes.0)?;
        Ok(manifest)
    }

    fn is_temporary(&self, path: &Path) -> bool {
        let Some(out_dir) = self.output.parent() else {
            return false;
        };
        let Ok(relative) = path.strip_prefix(out_dir) else {
            return false;
        };
        if !matches!(path.try_exists(), Ok(false)) {
            return false;
        }
        let mut components = relative.iter();
        let Some(first) = components.next().and_then(OsStr::to_str) else {
            return false;
        };
        match (components.next().and_then(OsStr::to_str), components.next()) {
            (None, None) => {
                let stem = self
                    .output
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .and_then(|s| s.strip_prefix("lib"));
                first.ends_with(".rcgu.o")
                    && stem.is_some_and(|stem| {
                        first
                            .strip_prefix(stem)
                            .is_some_and(|suffix| suffix.starts_with('.'))
                    })
            }
            (Some("list" | "symbols.o" | "rmeta.o" | "metadata.o"), None) => {
                first.strip_prefix("rustc").is_some_and(|suffix| {
                    suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
                })
            }
            _ => false,
        }
    }
}

pub(crate) struct Capture {
    _dir: TempDir,
    pub(crate) path: PathBuf,
    start: SystemTime,
}

struct LimitedManifest(Vec<u8>);

impl Write for LimitedManifest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_MANIFEST_LEN - self.0.len() {
            return Err(io::Error::other("oversized link manifest"));
        }
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    files: Vec<Input>,
    names: BTreeSet<String>,
    searches: Vec<Search>,
}

#[derive(Serialize, Deserialize)]
struct Input {
    path: LinkPath,
    size: u64,
    mtime: (i64, i64),
    digest: String,
}

#[derive(Serialize, Deserialize)]
struct Search {
    path: LinkPath,
    native: bool,
    modified: Option<SystemTime>,
    entries: BTreeMap<String, String>,
}

#[derive(Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
enum LinkPath {
    Basedir(usize, PathBuf),
    Sysroot(PathBuf),
    Absolute(PathBuf),
}

impl LinkPath {
    fn new(path: &Path, basedirs: &[Vec<u8>], sysroot: Option<&Path>) -> Self {
        if !path.components().any(|part| part == Component::ParentDir) {
            if let Some(root) = sysroot
                && let Ok(relative) = path.strip_prefix(root)
            {
                return Self::Sysroot(relative.to_owned());
            }
            if let Some((index, _, relative)) = basedirs
                .iter()
                .enumerate()
                .filter_map(|(index, base)| {
                    let root = Path::new(std::str::from_utf8(base).ok()?);
                    Some((index, base.len(), path.strip_prefix(root).ok()?))
                })
                .max_by_key(|(_, len, _)| *len)
            {
                return Self::Basedir(index, relative.to_owned());
            }
        }
        Self::Absolute(path.to_owned())
    }

    fn resolve(&self, compilation: &LinkedCompilation) -> Result<PathBuf> {
        let (root, relative) = match self {
            Self::Basedir(index, relative) => {
                let base = compilation
                    .basedirs
                    .get(*index)
                    .context("missing link manifest basedir")?;
                (Path::new(std::str::from_utf8(base)?), relative)
            }
            Self::Sysroot(relative) => (&*compilation.sysroot, relative),
            Self::Absolute(path) => {
                ensure!(path.is_absolute(), "relative absolute link manifest path");
                return Ok(path.clone());
            }
        };
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
            "invalid relative link manifest path"
        );
        Ok(root.join(relative))
    }
}

fn parse_depfile(text: &str, output: &Path, cwd: &Path) -> Result<BTreeSet<PathBuf>> {
    let (rule, phony_targets) = text
        .split_once("\n\n")
        .context("incomplete linker depfile")?;
    let has_continuations = rule.contains("\\\n");
    let rule = [Some(output), output.strip_prefix(cwd).ok()]
        .into_iter()
        .flatten()
        .filter_map(Path::to_str)
        .find_map(|target| rule.strip_prefix(target)?.strip_prefix(':'))
        .context("unexpected linker depfile target")?
        .replace("\\\n", "");
    let mut rest = rule.trim_start();
    let mut paths = BTreeSet::new();
    for target in phony_targets.lines().filter(|line| !line.is_empty()) {
        let name = target
            .strip_suffix(':')
            .context("invalid linker depfile target")?;
        // LLD escapes both rules. BFD escapes the prerequisites and mold
        // writes them verbatim. Read one path at a time even if it has spaces.
        let escaped = name.replace(' ', "\\ ");
        let (raw, remaining) = [name, &escaped]
            .into_iter()
            .find_map(|candidate| {
                let remaining = rest.strip_prefix(candidate)?;
                (remaining.is_empty() || remaining.starts_with(char::is_whitespace))
                    .then_some((candidate == name, remaining))
            })
            .context("linker depfile rules disagree")?;
        let name = if raw && has_continuations {
            name.replace("\\ ", " ")
        } else {
            name.to_owned()
        };
        ensure!(
            !name.contains(['\\', '$', '#', '\0', '\t']),
            "unsupported linker depfile path"
        );
        paths.insert(cwd.join(name));
        rest = remaining.trim_start();
    }
    ensure!(
        rest.is_empty() && !paths.is_empty(),
        "incomplete linker depfile rule"
    );
    Ok(paths)
}

fn shadow_names(paths: &BTreeSet<PathBuf>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if let Some(extension) = path.extension().and_then(OsStr::to_str)
            && matches!(extension, "a" | "so")
        {
            let stem = path.file_stem().unwrap().to_str().unwrap();
            names.insert(format!("{stem}.a"));
            names.insert(format!("{stem}.so"));
        }
        names.insert(name.to_owned());
    }
    names
}

fn directory_mtime(path: &Path) -> Result<Option<SystemTime>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.modified()?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn search_snapshot(
    dir: &Path,
    names: &BTreeSet<String>,
    native: bool,
    excluded_output: &Path,
) -> Result<BTreeMap<String, String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(err.into()),
    };
    let mut result = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        let Some(name) = name.to_str() else {
            ensure!(
                !native || path.extension() != Some(OsStr::new("a")),
                "non-UTF-8 native archive"
            );
            continue;
        };
        let native_input =
            native && (name.ends_with(".a") || name.ends_with(".so") || name.contains(".so."));
        if path != excluded_output && (names.contains(name) || native_input) {
            match Stamp::of(&path).and_then(|stamp| file_digest(&path, &stamp)) {
                Ok(digest) => {
                    result.insert(name.to_owned(), digest);
                }
                // Some system search directories include broken symlinks.
                Err(error)
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(result)
}

#[cfg(all(test, target_os = "linux"))]
mod test {
    use super::*;
    use crate::cache::CacheWrite;
    use std::io::Cursor;

    #[test]
    fn test_linked_key_hashes_tools_and_link_environment() {
        let tool = ToolIdentity {
            digest: "driver digest".into(),
            version: (b"driver version".into(), vec![]),
            gnu_depfile: true,
        };
        let identity = Identity {
            driver: tool.clone(),
            linkers: vec![tool],
            uses_driver: true,
        };
        let key = |identity: &Identity, env: &[(OsString, OsString)]| {
            let mut digest = Digest::new();
            identity.hash_key(&mut digest, env, &[]);
            digest.finish()
        };
        let baseline = key(&identity, &[]);
        let mut changed = identity.clone();
        changed.driver.digest.push('2');
        assert_ne!(baseline, key(&changed, &[]));
        changed = identity.clone();
        changed.driver.version.0.push(b'2');
        assert_ne!(baseline, key(&changed, &[]));
        changed = identity.clone();
        changed.linkers[0].digest.push('2');
        assert_ne!(baseline, key(&changed, &[]));
        changed = identity.clone();
        changed.linkers[0].version.1.push(b'2');
        assert_ne!(baseline, key(&changed, &[]));
        for var in LINK_ENV
            .iter()
            .copied()
            .chain(["MOLD_JOBS", "LLD_REPRODUCE"])
        {
            let empty = key(&identity, &[(var.into(), "".into())]);
            let value = key(&identity, &[(var.into(), "/abs/path".into())]);
            assert_ne!(baseline, empty, "{var}");
            assert_ne!(empty, value, "{var}");
        }
        assert_eq!(
            baseline,
            key(&identity, &[("IGNORED".into(), "value".into())])
        );
    }

    #[test]
    fn test_linked_depfiles() {
        let cwd = Path::new("/work");
        let output = cwd.join("libpm.so");
        let cases = [
            include_str!("fixtures/rust-lld.d"),
            include_str!("fixtures/mold.d"),
            include_str!("fixtures/gnu-ld.d"),
        ];
        for (index, depfile) in cases.iter().enumerate() {
            assert_eq!(
                parse_depfile(depfile, &output, cwd).unwrap(),
                [
                    cwd.join("rustcXXXXXX/list"),
                    cwd.join("pm.pm-cgu.0.rcgu.o"),
                    cwd.join("native archive/libanswer.a"),
                    PathBuf::from(if index == 0 {
                        "/usr/lib/x86_64-linux-gnu/libc.so"
                    } else {
                        "/lib/x86_64-linux-gnu/libc.so"
                    }),
                ]
                .into_iter()
                .collect()
            );
        }
        assert!(parse_depfile(cases[0], &cwd.join("other.so"), cwd).is_err());
        assert!(
            parse_depfile(
                cases[0].trim_end_matches("/work/native\\ archive/libanswer.a:\n"),
                &output,
                cwd
            )
            .is_err()
        );
    }

    #[test]
    fn test_linked_manifest_verifies_remapped_files_and_search_paths() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let compilation = |cwd: &Path| LinkedCompilation {
            available: Some(SearchPaths {
                directories: vec![(cwd.join("earlier"), true)],
                tools: vec![],
            }),
            uses_driver: true,
            output: cwd.join("libpm.so"),
            cwd: cwd.to_owned(),
            sysroot: PathBuf::from("/sysroot"),
            basedirs: crate::config::basedirs_from_env(cwd.as_os_str()).unwrap(),
        };
        let a = compilation(a.path());
        let b = compilation(b.path());
        for root in [&a.cwd, &b.cwd] {
            fs::create_dir(root.join("earlier")).unwrap();
            fs::write(root.join("libanswer.a"), "original").unwrap();
        }
        assert!(a.is_temporary(&a.cwd.join("pm.pm-cgu.0.rcgu.o")));
        assert!(a.is_temporary(&a.cwd.join("rustcXXXXXX/list")));
        assert!(!a.is_temporary(&a.cwd.join("rustcXXXXXX/libanswer.a")));
        assert!(!a.is_temporary(&a.cwd.join("other.rcgu.o")));
        assert_eq!(
            LinkPath::new(&a.cwd.join("libanswer.a"), &a.basedirs, Some(&a.sysroot))
                .resolve(&b)
                .unwrap(),
            b.cwd.join("libanswer.a")
        );

        let capture = a.capture().unwrap();
        assert!(a.finish_capture(&capture).is_err());
        fs::write(
            &capture.path,
            format!(
                "{}: pm.pm-cgu.0.rcgu.o rustcXXXXXX/list libanswer.a\n\npm.pm-cgu.0.rcgu.o:\n\nrustcXXXXXX/list:\n\nlibanswer.a:\n",
                a.output.display()
            ),
        ).unwrap();
        let file = a.finish_capture(&capture).unwrap();
        let mut writer = CacheWrite::new();
        writer
            .put_object(MEMBER, &mut File::open(file.path()).unwrap(), None)
            .unwrap();
        let mut entry = CacheRead::from(Cursor::new(writer.finish().unwrap())).unwrap();

        b.verify(&mut entry).unwrap();
        fs::write(b.cwd.join("earlier/libanswer.so"), "shadows").unwrap();
        assert!(b.verify(&mut entry).is_err());
        fs::remove_file(b.cwd.join("earlier/libanswer.so")).unwrap();
        b.verify(&mut entry).unwrap();
        let path = b.cwd.join("libanswer.a");
        fs::write(&path, "changed!").unwrap();
        assert!(b.verify(&mut entry).is_err());
        fs::write(&path, b"!<thin>\n").unwrap();
        assert!(b.verify(&mut entry).is_err());
        fs::remove_file(path).unwrap();
        assert!(b.verify(&mut entry).is_err());
    }
}
