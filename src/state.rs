use std::{
    collections::{BTreeMap, HashSet},
    env,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Record {
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
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
    pub fn open_readonly() -> Result<Self> {
        Ok(Self {
            root: state_root()?,
        })
    }

    pub fn open() -> Result<Self> {
        let root = state_root()?;
        fs::create_dir_all(root.join("records")).context("create wt state directory")?;
        Ok(Self { root })
    }

    pub fn lock(&self) -> Result<Lock> {
        self.lock_file(&self.root.join("lock"))
    }

    pub fn lock_worktree(&self, repository: &str, reference: &str) -> Result<Lock> {
        let record = match reference.parse::<u64>() {
            Ok(issue) => self.issue_path(repository, issue),
            Err(_) => self.branch_path(repository, reference),
        };
        let directory = self.root.join("locks");
        fs::create_dir_all(&directory)?;
        self.lock_file(&directory.join(record.file_name().context("record has no filename")?))
    }

    fn lock_file(&self, path: &Path) -> Result<Lock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
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

    pub fn find_issue(&self, repository: &str, issue: u64) -> Result<Option<Record>> {
        self.read_record(self.issue_path(repository, issue))
    }

    pub fn find_branch(&self, repository: &str, branch: &str) -> Result<Option<Record>> {
        let record = self.read_record(self.branch_path(repository, branch))?;
        if record.as_ref().is_some_and(|record| {
            !record.repository.eq_ignore_ascii_case(repository)
                || record.issue.is_some()
                || record.branch != branch
        }) {
            bail!("state key collision for branch {branch}");
        }
        Ok(record)
    }

    fn read_record(&self, path: PathBuf) -> Result<Option<Record>> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        Ok(Some(
            serde_json::from_slice(&bytes).context("parse wt state")?,
        ))
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        let path = self.record_path(record);
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(record)?)?;
        fs::rename(temporary, path).context("save wt state atomically")
    }

    pub fn delete(&self, record: &Record) -> Result<()> {
        fs::remove_file(self.record_path(record)).context("remove wt state")
    }

    pub fn records(&self) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        let entries = match fs::read_dir(self.root.join("records")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
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
        let path = self.trust_path(repository);
        let mut hashes = fs::read_to_string(&path).unwrap_or_default();
        if !hashes.lines().any(|trusted| trusted == hash) {
            hashes.push_str(hash);
            hashes.push('\n');
            fs::write(path, hashes)?;
        }
        Ok(())
    }

    pub fn is_trusted(&self, repository: &str, hash: &str) -> Result<bool> {
        Ok(fs::read_to_string(self.trust_path(repository))
            .unwrap_or_default()
            .lines()
            .any(|trusted| trusted == hash))
    }

    fn record_path(&self, record: &Record) -> PathBuf {
        match record.issue {
            Some(issue) => self.issue_path(&record.repository, issue),
            None => self.branch_path(&record.repository, &record.branch),
        }
    }

    fn issue_path(&self, repository: &str, issue: u64) -> PathBuf {
        matching_path(
            &self.root.join("records"),
            format!("{}--{issue}.json", repository.replace('/', "--")),
        )
    }

    fn branch_path(&self, repository: &str, branch: &str) -> PathBuf {
        matching_path(
            &self.root.join("records"),
            format!(
                "{}--branch-{:016x}.json",
                repository.replace('/', "--"),
                branch_key(branch)
            ),
        )
    }

    fn trust_path(&self, repository: &str) -> PathBuf {
        matching_path(&self.root.join("trust"), repository.replace('/', "--"))
    }
}

fn branch_key(branch: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    branch
        .as_bytes()
        .iter()
        .fold(FNV_OFFSET_BASIS, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
        })
}

fn matching_path(directory: &Path, name: String) -> PathBuf {
    let path = directory.join(&name);
    if path.exists() {
        return path;
    }
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(&name)
        })
        .map(|entry| entry.path())
        .unwrap_or(path)
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
