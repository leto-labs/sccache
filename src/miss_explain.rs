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

use crate::errors::*;
use crate::util::{Digest, HashToDigest};
use fs_err as fs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::hash::Hash;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::Mutex;

pub(crate) const DIRECTORY: &str = "explain";
const HISTORY_SIZE: usize = 4;
const MAX_LISTED_FEATURES: usize = 10;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Component {
    pub name: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Manifest {
    pub identity: String,
    pub crate_name: String,
    pub cwd: String,
    pub basedir: Option<String>,
    pub compilation: Compilation,
    pub artifacts: Vec<String>,
    pub components: Vec<Component>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Compilation {
    #[serde(skip_serializing_if = "Option::is_none")]
    out_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    crate_types: Vec<String>,
    is_test: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_kind: Option<&'static str>,
    #[serde(flatten)]
    features: FeatureSet,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum FeatureSet {
    Features(Vec<String>),
    FeaturesHash(String),
}

impl Compilation {
    pub(crate) fn new<'a>(
        cwd: &Path,
        out_dir: &Path,
        basedir: Option<&Path>,
        crate_name: &str,
        crate_types: Vec<&str>,
        cfgs: impl Iterator<Item = &'a OsStr>,
        target: Option<&str>,
    ) -> Self {
        let resolved = cwd.join(out_dir);
        let relative = basedir
            .and_then(|dir| resolved.strip_prefix(dir).ok())
            .unwrap_or(out_dir);
        let (profile, layout_kind) = cargo_layout(&resolved, crate_name, &crate_types);

        let mut is_test = false;
        let mut features = Vec::new();
        for cfg in cfgs.filter_map(OsStr::to_str).map(str::trim) {
            is_test |= cfg == "test";
            if let Some((name, value)) = cfg.split_once('=')
                && name.trim() == "feature"
                && let Some(feature) = value
                    .trim()
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
            {
                features.push(feature);
            }
        }
        features.sort_unstable();
        features.dedup();
        // A hand-written cfg can contain arbitrary strings, including paths.
        let features = if features.len() > MAX_LISTED_FEATURES
            || features.iter().any(|feature| {
                feature.contains(['/', '\\', ':']) || feature.chars().any(char::is_control)
            }) {
            let mut digest = Digest::new();
            for feature in features {
                digest.update(feature.as_bytes());
                digest.update(b"\0");
            }
            FeatureSet::FeaturesHash(digest.finish())
        } else {
            FeatureSet::Features(features.into_iter().map(str::to_owned).collect())
        };

        let target_kind = layout_kind.or(match crate_types.as_slice() {
            _ if is_test => Some("test"),
            ["bin"] => Some("bin"),
            ["proc-macro"] => Some("proc-macro"),
            [] => None,
            _ => Some("lib"),
        });
        Self {
            out_dir: display_out_dir(relative),
            profile: profile.map(str::to_owned),
            crate_types: crate_types.into_iter().map(str::to_owned).collect(),
            is_test,
            target: target
                .filter(|name| {
                    name.starts_with(|ch: char| ch.is_ascii_alphanumeric())
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                })
                .map(str::to_owned),
            target_kind,
            features,
        }
    }
}

fn display_out_dir(path: &Path) -> Option<String> {
    if path
        .components()
        .any(|part| !matches!(part, PathComponent::Normal(_) | PathComponent::CurDir))
    {
        return None;
    }
    let path = path.to_string_lossy();
    #[cfg(windows)]
    let path = path.replace('\\', "/");
    Some(if path.is_empty() {
        ".".into()
    } else {
        path.into()
    })
}

fn cargo_layout<'a>(
    out_dir: &'a Path,
    crate_name: &str,
    crate_types: &[&str],
) -> (Option<&'a str>, Option<&'static str>) {
    let parent = out_dir.parent();
    let package = if out_dir.ends_with("out") {
        parent
    } else {
        Some(out_dir)
    };
    let (profile_dir, target_kind) = if out_dir.ends_with("out")
        && let Some(build) = package.and_then(Path::parent).and_then(Path::parent)
        && build.ends_with("build")
    {
        // Newer Cargo uses build/<package>/<unit>/out for all targets.
        let is_build_script = crate_types == ["bin"] && crate_name.starts_with("build_script_");
        (build.parent(), is_build_script.then_some("build-script"))
    } else if let Some(build) = package.and_then(Path::parent)
        && build.ends_with("build")
    {
        (build.parent(), Some("build-script"))
    } else if out_dir.ends_with("examples") {
        (parent, Some("example"))
    } else if out_dir.ends_with("deps") {
        (parent, None)
    } else {
        (None, None)
    };
    let profile = profile_dir
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .filter(|name| {
            name.chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
        })
        .map(|name| if name == "debug" { "dev" } else { name });
    (profile, target_kind)
}

impl Manifest {
    pub fn add(&mut self, name: impl Into<String>, digest: String) {
        self.components.push(Component {
            name: name.into(),
            digest,
            artifact: None,
        });
    }

    pub(crate) fn add_argument(&mut self, arg: &OsStr, value: Option<&OsStr>) {
        let flag = arg.to_string_lossy();
        let name = match (flag.as_ref(), value) {
            ("--cfg", Some(cfg)) => format!("arg:--cfg {}", cfg.to_string_lossy()),
            ("-C" | "--codegen" | "-Z", Some(option)) => {
                let option = option.to_string_lossy();
                let key = option
                    .split_once('=')
                    .map_or(option.as_ref(), |(key, _)| key);
                format!("arg:{flag} {key}")
            }
            (flag, _) if flag.starts_with('-') => {
                let name = flag.split_once('=').map_or(flag, |(name, _)| name);
                format!("arg:{name}")
            }
            _ => "arg:input".into(),
        };
        self.add(name, digest((arg, value)));
    }
}

pub(crate) fn digest(value: impl Hash) -> String {
    let mut digest = Digest::new();
    value.hash(&mut HashToDigest {
        digest: &mut digest,
    });
    digest.finish()
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry<T> {
    key: String,
    components: T,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Classification {
    New,
    OwnInput,
    Dependency,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MissExplainStats {
    pub new: u64,
    pub own_input: u64,
    pub dependency: u64,
    pub unchanged: u64,
}

impl MissExplainStats {
    pub(crate) fn increment(&mut self, classification: Classification) {
        *match classification {
            Classification::New => &mut self.new,
            Classification::OwnInput => &mut self.own_input,
            Classification::Dependency => &mut self.dependency,
            Classification::Unchanged => &mut self.unchanged,
        } += 1;
    }
}

impl std::ops::AddAssign for MissExplainStats {
    fn add_assign(&mut self, rhs: Self) {
        self.new += rhs.new;
        self.own_input += rhs.own_input;
        self.dependency += rhs.dependency;
        self.unchanged += rhs.unchanged;
    }
}

#[derive(Debug, Serialize)]
struct Difference<'a> {
    name: &'a str,
    previous: Option<&'a str>,
    current: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact: Option<&'a str>,
}

fn differences<'a>(previous: &'a [Component], current: &'a [Component]) -> Vec<Difference<'a>> {
    let mut by_name = BTreeMap::<_, (Vec<_>, Vec<_>)>::new();
    for component in previous {
        by_name
            .entry(component.name.as_str())
            .or_default()
            .0
            .push(component);
    }
    for component in current {
        by_name
            .entry(component.name.as_str())
            .or_default()
            .1
            .push(component);
    }
    let mut changes = Vec::new();
    for (name, (old, new)) in by_name {
        for i in 0..old.len().max(new.len()) {
            let previous = old.get(i).map(|component| component.digest.as_str());
            let current = new.get(i).map(|component| component.digest.as_str());
            if previous != current {
                changes.push(Difference {
                    name,
                    previous,
                    current,
                    artifact: new
                        .get(i)
                        .and_then(|component| component.artifact.as_deref()),
                });
            }
        }
    }
    changes
}

#[derive(Serialize)]
struct Explanation<'a> {
    time: String,
    crate_name: &'a str,
    identity: &'a str,
    cwd: &'a str,
    basedir: Option<&'a str>,
    #[serde(flatten)]
    compilation: &'a Compilation,
    artifacts: &'a [String],
    key: &'a str,
    closest_key: Option<&'a str>,
    classification: Classification,
    differences: Vec<Difference<'a>>,
}

impl<'a> Explanation<'a> {
    fn new(key: &'a str, manifest: &'a Manifest, history: &'a [Entry<Vec<Component>>]) -> Self {
        let exact = history.iter().find(|entry| entry.key == key);
        let (closest_key, classification, differences) = if let Some(entry) = exact {
            (
                Some(entry.key.as_str()),
                Classification::Unchanged,
                Vec::new(),
            )
        } else if let Some((entry, mut differences)) = history
            .iter()
            .map(|entry| (entry, differences(&entry.components, &manifest.components)))
            .min_by_key(|(_, differences)| differences.len())
        {
            let classification = if !differences.is_empty()
                && differences
                    .iter()
                    .all(|item| item.name.starts_with("extern:"))
            {
                Classification::Dependency
            } else {
                Classification::OwnInput
            };
            if differences.is_empty() {
                // The key hashes inputs in order, while the diff pairs them by name.
                differences.push(Difference {
                    name: "input-order",
                    previous: Some(&entry.key),
                    current: Some(key),
                    artifact: None,
                });
            }
            (Some(entry.key.as_str()), classification, differences)
        } else {
            (None, Classification::New, Vec::new())
        };
        Self {
            time: chrono::Utc::now().to_rfc3339(),
            crate_name: &manifest.crate_name,
            identity: &manifest.identity,
            cwd: &manifest.cwd,
            basedir: manifest.basedir.as_deref(),
            compilation: &manifest.compilation,
            artifacts: &manifest.artifacts,
            key,
            closest_key,
            classification,
            differences,
        }
    }
}

pub(crate) struct MissExplain {
    root: PathBuf,
    log: PathBuf,
    log_lock: tokio::sync::Mutex<()>,
    history_lock: Mutex<()>,
}

impl MissExplain {
    pub fn new(cache: &Path, log: PathBuf) -> Self {
        Self {
            root: cache.join(DIRECTORY),
            log,
            log_lock: tokio::sync::Mutex::new(()),
            history_lock: Mutex::new(()),
        }
    }

    fn path(&self, identity: &str) -> PathBuf {
        self.root
            .join(&identity[..2])
            .join(format!("{identity}.json"))
    }

    fn read(path: &Path) -> Result<Vec<Entry<Vec<Component>>>> {
        match fs::File::open(path) {
            Ok(file) => Ok(serde_json::from_reader(BufReader::new(file))?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn prepare(&self, key: &str, manifest: &Manifest) -> Result<(Classification, Vec<u8>)> {
        let history = Self::read(&self.path(&manifest.identity)).unwrap_or_else(|err| {
            debug!("Could not read Rust miss history: {err:#}");
            Vec::new()
        });
        let explanation = Explanation::new(key, manifest, &history);
        let mut line = serde_json::to_vec(&explanation)?;
        line.push(b'\n');
        Ok((explanation.classification, line))
    }

    pub async fn append(&self, line: Vec<u8>, pool: &tokio::runtime::Handle) -> Result<()> {
        // Wait outside the blocking pool so logging cannot occupy its workers
        // while another append holds the lock.
        let _lock = self.log_lock.lock().await;
        let path = self.log.clone();
        pool.spawn_blocking(move || Self::write_log(&path, &line))
            .await?
    }

    fn write_log(path: &Path, line: &[u8]) -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        let mut log = match options.open(path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                options.open(path)?
            }
            result => result?,
        };
        log.write_all(line)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn lock_log(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.log_lock.try_lock().unwrap()
    }

    pub fn remember(&self, key: String, manifest: &Manifest) -> Result<()> {
        let _lock = self
            .history_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let path = self.path(&manifest.identity);
        let mut history = Self::read(&path).unwrap_or_else(|err| {
            debug!("Could not read Rust miss history: {err:#}");
            Vec::new()
        });
        history.retain(|entry| entry.key != key);
        let entries: Vec<_> = std::iter::once(Entry {
            key,
            components: manifest.components.as_slice(),
        })
        .chain(history.iter().map(|entry| Entry {
            key: entry.key.clone(),
            components: entry.components.as_slice(),
        }))
        .take(HISTORY_SIZE)
        .collect();

        let dir = path.parent().unwrap();
        fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        let mut writer = BufWriter::new(file.as_file_mut());
        serde_json::to_writer(&mut writer, &entries)?;
        writer.into_inner().map_err(|err| err.into_error())?;
        file.persist(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use serde_json::{Value, json};

    fn manifest(components: &[(&str, &str)]) -> Manifest {
        let mut manifest = Manifest {
            identity: digest("example"),
            crate_name: "example".into(),
            cwd: "/work/example".into(),
            basedir: Some("/work".into()),
            compilation: Compilation::new(
                Path::new("/work/example"),
                Path::new("target/debug/deps"),
                None,
                "example",
                vec!["rlib"],
                std::iter::empty(),
                None,
            ),
            artifacts: vec!["libexample.rmeta".into()],
            components: Vec::new(),
        };
        for (name, digest) in components {
            manifest.add(*name, (*digest).into());
        }
        manifest
    }

    fn entry(key: &str, components: &[(&str, &str)]) -> Entry<Vec<Component>> {
        Entry {
            key: key.into(),
            components: manifest(components).components,
        }
    }

    fn compilation(out_dir: &str, cfgs: &[&str], target: Option<&str>) -> Value {
        let mut manifest = manifest(&[]);
        manifest.compilation = Compilation::new(
            Path::new("/work/example"),
            Path::new(out_dir),
            Some(Path::new("/work")),
            "example",
            vec!["rlib"],
            cfgs.iter().map(OsStr::new),
            target,
        );
        serde_json::to_value(Explanation::new("current", &manifest, &[])).unwrap()
    }

    #[test]
    fn records_only_relative_or_basedir_stripped_output_paths() {
        for (path, expected) in [
            (
                "/work/example/target/debug/deps",
                json!("example/target/debug/deps"),
            ),
            ("target/debug/deps", json!("example/target/debug/deps")),
            ("/work", json!(".")),
        ] {
            assert_eq!(compilation(path, &[], None)["out_dir"], expected);
        }
        for path in ["/work-other/target/debug/deps", "/work/../private/deps"] {
            assert!(compilation(path, &[], None).get("out_dir").is_none());
        }
        let external = compilation("/private/custom-target/fast/deps", &[], None);
        assert!(external.get("out_dir").is_none());
        assert_eq!(external["profile"], "fast");
    }

    #[test]
    fn derives_profiles_and_target_kinds_from_cargo_output_paths() {
        for (path, profile, kind) in [
            ("target/debug/deps", "dev", "lib"),
            ("target/release/deps", "release", "lib"),
            ("target/élan/deps", "élan", "lib"),
            (
                "build-output/aarch64-unknown-linux-gnu/fast/deps",
                "fast",
                "lib",
            ),
            ("custom-target/release/examples", "release", "example"),
            ("target/debug/build/pkg-abcd", "dev", "build-script"),
            (
                "custom-target/fast/build/pkg-abcd/out",
                "fast",
                "build-script",
            ),
            ("custom-target/fast/build/pkg/abcd/out", "fast", "lib"),
        ] {
            let record = compilation(path, &[], None);
            assert_eq!(record["profile"], profile, "{path}");
            assert_eq!(record["target_kind"], kind, "{path}");
        }
        assert!(
            compilation("arbitrary-output", &[], None)
                .get("profile")
                .is_none()
        );
        assert_eq!(
            cargo_layout(
                Path::new("custom-target/fast/build/pkg/abcd/out"),
                "build_script_custom",
                &["bin"],
            ),
            (Some("fast"), Some("build-script")),
        );
    }

    #[test]
    fn records_target_triple_and_test_cfg() {
        let target = "thumbv8m.main-none-eabi";
        let record = compilation("target/debug/deps", &["test"], Some(target));
        assert_eq!(record["is_test"], true);
        assert_eq!(record["target_kind"], "test");
        assert_eq!(record["crate_types"], json!(["rlib"]));
        assert_eq!(record["target"], target);
        let record = compilation(
            "target/debug/deps",
            &[r#"feature="test""#],
            Some("/private/custom-target"),
        );
        assert_eq!(record["is_test"], false);
        assert_eq!(record["features"], json!(["test"]));
        assert!(record.get("target").is_none());
    }

    #[test]
    fn lists_ten_distinct_features_and_hashes_larger_sets() {
        let names: Vec<_> = (0..11).map(|n| format!("feature-{n:02}")).collect();
        let cfgs: Vec<_> = names
            .iter()
            .map(|name| format!(r#"feature = "{name}""#))
            .collect();
        let mut inputs: Vec<_> = cfgs[..10].iter().rev().map(String::as_str).collect();
        inputs.extend([cfgs[0].as_str(), r#"unrelated="value""#]);
        let small = compilation("target/debug/deps", &inputs, None);
        assert_eq!(small["features"], json!(names[..10]));
        assert!(small.get("features_hash").is_none());

        inputs.push(&cfgs[10]);
        let large = compilation("target/debug/deps", &inputs, None);
        assert!(large.get("features").is_none());
        assert_eq!(large["features_hash"].as_str().unwrap().len(), 64);
        inputs.reverse();
        let reordered = compilation("target/debug/deps", &inputs, None);
        assert_eq!(reordered["features_hash"], large["features_hash"]);
        inputs[0] = r#"feature="different""#;
        let changed = compilation("target/debug/deps", &inputs, None);
        assert_ne!(changed["features_hash"], large["features_hash"]);

        let private = compilation("target/debug/deps", &[r#"feature="/private/file""#], None);
        assert!(private.get("features_hash").is_some());
        assert!(!private.to_string().contains("/private/file"));
    }

    #[test]
    fn diff_reports_changed_added_and_removed_components() {
        let old = manifest(&[("source:gone", "1"), ("env:VAR", "2"), ("env:VAR", "3")]);
        let new = manifest(&[("env:VAR", "2"), ("env:VAR", "4"), ("source:new", "5")]);
        let diff = differences(&old.components, &new.components);
        let changes: Vec<_> = diff
            .iter()
            .map(|d| (d.name, d.previous, d.current))
            .collect();
        assert_eq!(
            changes,
            [
                ("env:VAR", Some("3"), Some("4")),
                ("source:gone", Some("1"), None),
                ("source:new", None, Some("5")),
            ]
        );
    }

    #[test]
    fn diff_reports_changed_rust_flags() {
        let args = |args: &[(&str, &str)]| {
            let mut manifest = manifest(&[]);
            for (flag, value) in args {
                manifest.add_argument(OsStr::new(flag), Some(OsStr::new(value)));
            }
            manifest.components
        };
        let old = args(&[
            ("--cfg", r#"feature="x""#),
            ("--cfg", "unix"),
            ("-C", "metadata=abcd"),
            ("-C", "opt-level=2"),
            ("--edition", "2024"),
        ]);
        let reordered = args(&[
            ("--edition", "2024"),
            ("-C", "opt-level=2"),
            ("--cfg", "unix"),
            ("-C", "metadata=abcd"),
            ("--cfg", r#"feature="x""#),
        ]);
        assert!(differences(&old, &reordered).is_empty());

        let new = args(&[
            ("--edition", "2024"),
            ("-C", "opt-level=3"),
            ("--cfg", "unix"),
            ("-C", "metadata=abcd"),
            ("--cfg", r#"feature="y""#),
        ]);
        let diff = differences(&old, &new);
        let changes: Vec<_> = diff
            .iter()
            .map(|item| (item.name, item.previous.is_some(), item.current.is_some()))
            .collect();
        assert_eq!(
            changes,
            [
                (r#"arg:--cfg feature="x""#, true, false),
                (r#"arg:--cfg feature="y""#, false, true),
                ("arg:-C opt-level", true, true),
            ]
        );
    }

    #[test]
    fn explains_against_closest_key_and_prefers_newest_tie() {
        let current = manifest(&[("source:a", "1"), ("extern:dep", "4")]);
        let history = [
            entry("far", &[("source:a", "0"), ("extern:dep", "0")]),
            entry("closer", &[("source:a", "1"), ("extern:dep", "2")]),
            entry("older", &[("source:a", "1"), ("extern:dep", "3")]),
        ];
        let explanation = Explanation::new("current", &current, &history);
        assert_eq!(explanation.closest_key, Some("closer"));
        assert_eq!(explanation.classification, Classification::Dependency);
        assert_eq!(explanation.differences[0].name, "extern:dep");
        assert_eq!(
            Explanation::new("closer", &current, &history).classification,
            Classification::Unchanged
        );
        assert_eq!(
            Explanation::new("current", &current, &history[..1]).classification,
            Classification::OwnInput
        );
        assert_eq!(
            Explanation::new("current", &current, &[]).classification,
            Classification::New
        );
    }

    #[test]
    fn remembers_four_distinct_keys_and_refreshes_an_existing_key() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("misses.jsonl");
        let store = MissExplain::new(dir.path(), log.clone());
        let manifest = manifest(&[("source:a", "1")]);
        for key in ["one", "two", "three", "four", "one", "five"] {
            store.remember(key.into(), &manifest)?;
        }
        let history = MissExplain::read(&store.path(&manifest.identity))?;
        assert_eq!(
            history
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            ["five", "one", "four", "three"],
        );
        for _ in 0..2 {
            let (classification, line) = store.prepare("one", &manifest)?;
            assert_eq!(classification, Classification::Unchanged);
            MissExplain::write_log(&log, &line)?;
        }
        assert_eq!(fs::read_to_string(log)?.lines().count(), 2);
        Ok(())
    }
}
