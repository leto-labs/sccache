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
use std::hash::Hash;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) const DIRECTORY: &str = "explain";
const HISTORY_SIZE: usize = 4;

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
    pub artifacts: Vec<String>,
    pub components: Vec<Component>,
}

impl Manifest {
    pub fn add(&mut self, name: impl Into<String>, digest: String) {
        self.components.push(Component {
            name: name.into(),
            digest,
            artifact: None,
        });
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
    log_lock: Mutex<()>,
    history_lock: Mutex<()>,
}

impl MissExplain {
    pub fn new(cache: &Path, log: PathBuf) -> Self {
        Self {
            root: cache.join(DIRECTORY),
            log,
            log_lock: Mutex::new(()),
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

    pub fn record(&self, key: &str, manifest: &Manifest) -> Result<Classification> {
        let history = Self::read(&self.path(&manifest.identity)).unwrap_or_else(|err| {
            debug!("Could not read Rust miss history: {err:#}");
            Vec::new()
        });
        let explanation = Explanation::new(key, manifest, &history);
        let mut line = serde_json::to_vec(&explanation)?;
        line.push(b'\n');
        let _lock = self.log_lock.lock().unwrap_or_else(|err| err.into_inner());
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)?
            .write_all(&line)?;
        Ok(explanation.classification)
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

    fn manifest(components: &[(&str, &str)]) -> Manifest {
        let mut manifest = Manifest {
            identity: digest("example"),
            crate_name: "example".into(),
            cwd: "/work/example".into(),
            basedir: Some("/work".into()),
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
        assert_eq!(store.record("one", &manifest)?, Classification::Unchanged);
        assert_eq!(store.record("one", &manifest)?, Classification::Unchanged);
        assert_eq!(fs::read_to_string(log)?.lines().count(), 2);
        Ok(())
    }
}
