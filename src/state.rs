use std::{
    collections::{BTreeMap, HashSet},
    env,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Record {
    pub repository: String,
    pub issue: u64,
    pub branch: String,
    pub path: PathBuf,
    pub ports: BTreeMap<String, u16>,
    pub copied_files: BTreeMap<PathBuf, String>,
    pub teardown: Option<String>,
    pub disposable: Vec<PathBuf>,
    #[serde(default)]
    pub config_hash: Option<String>,
}

pub struct Store {
    root: PathBuf,
}

pub struct Lock {
    _file: File,
}

impl Store {
    pub fn open() -> Result<Self> {
        let root = state_root()?;
        fs::create_dir_all(root.join("records")).context("create wt state directory")?;
        Ok(Self { root })
    }

    pub fn lock(&self) -> Result<Lock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("lock"))
            .context("open wt state lock")?;
        fs4::FileExt::lock(&file).context("lock wt state")?;
        Ok(Lock { _file: file })
    }

    pub fn used_ports(&self) -> Result<HashSet<u16>> {
        Ok(self
            .records()?
            .into_iter()
            .flat_map(|record| record.ports.into_values())
            .collect())
    }

    pub fn find(&self, repository: &str, issue: u64) -> Result<Option<Record>> {
        let path = self.record_path(repository, issue);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(
            serde_json::from_slice(&bytes).context("parse wt state")?,
        ))
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        let path = self.record_path(&record.repository, record.issue);
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(record)?)?;
        fs::rename(temporary, path).context("save wt state atomically")
    }

    pub fn delete(&self, repository: &str, issue: u64) -> Result<()> {
        fs::remove_file(self.record_path(repository, issue)).context("remove wt state")
    }

    pub fn records(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(self.root.join("records"))? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                records.push(serde_json::from_slice(&fs::read(path)?)?);
            }
        }
        Ok(records)
    }

    pub fn trust(&self, repository: &str, hash: &str) -> Result<()> {
        let directory = self.root.join("trust");
        fs::create_dir_all(&directory)?;
        let path = directory.join(repository.replace('/', "--"));
        let mut hashes = fs::read_to_string(&path).unwrap_or_default();
        if !hashes.lines().any(|trusted| trusted == hash) {
            hashes.push_str(hash);
            hashes.push('\n');
            fs::write(path, hashes)?;
        }
        Ok(())
    }

    pub fn is_trusted(&self, repository: &str, hash: &str) -> Result<bool> {
        let path = self.root.join("trust").join(repository.replace('/', "--"));
        Ok(fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .any(|trusted| trusted == hash))
    }

    fn record_path(&self, repository: &str, issue: u64) -> PathBuf {
        let repository = repository.replace('/', "--");
        self.root
            .join("records")
            .join(format!("{repository}--{issue}.json"))
    }
}

fn state_root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("WT_STATE_HOME") {
        return Ok(path.into());
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("wt"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".local/state/wt"))
}
