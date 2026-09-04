use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

#[derive(Default)]
pub struct Config {
    pub base: Option<String>,
    pub env: Option<PathBuf>,
    pub copies: Vec<PathBuf>,
    pub compose: bool,
    pub ports: Vec<String>,
    pub bootstrap: Option<String>,
    pub teardown: Option<String>,
    pub disposable: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub key: String,
    pub default: Option<u16>,
}

pub fn worktree_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("WT_WORKTREE_ROOT") {
        return Ok(root.into());
    }
    let path = global_config_path()?;
    if let Some(root) = one(&path, "wt.root")? {
        return Ok(PathBuf::from(root));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Developer/worktrees"))
}

pub fn write_worktree_root(root: &Path) -> Result<()> {
    if !root.is_absolute() {
        bail!("worktree root must be an absolute path");
    }
    let path = global_config_path()?;
    fs::create_dir_all(path.parent().context("global config has no parent")?)?;
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(&path)
        .args(["wt.root"])
        .arg(root)
        .output()
        .context("write global wt config")?;
    if !output.status.success() {
        bail!(
            "cannot write {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn global_config_path() -> Result<PathBuf> {
    if let Some(root) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(root).join("wt/config"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/wt/config"))
}

impl Config {
    pub fn load(repo: &Path) -> Result<Self> {
        let path = repo.join(".wtconfig");
        if !path.exists() {
            return Ok(Self::default());
        }
        let entries = entries(&path)?;
        let all = |key: &str| {
            entries
                .iter()
                .filter(|(name, _)| name == key)
                .map(|(_, value)| value.to_owned())
                .collect::<Vec<_>>()
        };
        let one = |key: &str| {
            entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.to_owned())
        };
        let config = Self {
            base: one("wt.base"),
            env: one("wt.env").map(PathBuf::from),
            copies: all("wt.copy").into_iter().map(PathBuf::from).collect(),
            compose: one("wt.compose").is_some_and(|value| is_true(&value)),
            ports: all("wt.port"),
            bootstrap: one("wt.bootstrap"),
            teardown: one("wt.teardown"),
            disposable: all("wt.disposable")
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        };
        config.validate_paths()?;
        Ok(config)
    }

    pub fn copied_files(&self) -> Vec<PathBuf> {
        let mut paths = self.env.iter().cloned().collect::<Vec<_>>();
        for path in &self.copies {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        paths
    }

    pub fn command_fingerprint(&self) -> Result<Option<String>> {
        if self.bootstrap.is_none() && self.teardown.is_none() {
            return Ok(None);
        }
        Ok(Some(serde_json::to_string(&[
            self.bootstrap.as_deref(),
            self.teardown.as_deref(),
        ])?))
    }

    pub fn write(&self, repo: &Path) -> Result<()> {
        fs::write(repo.join(".wtconfig"), self.render()).context("write .wtconfig")
    }

    pub fn validate_for_write(&self) -> Result<()> {
        self.validate_paths()?;
        if self.compose && self.env.is_none() {
            bail!("wt.compose requires --env");
        }
        for port in &self.ports {
            let spec = parse_port_spec(port)?;
            if spec.default.is_none() && self.env.is_none() {
                bail!("wt.port = {port} requires --env or an explicit default such as {port}:3000");
            }
        }
        for command in [&self.bootstrap, &self.teardown].into_iter().flatten() {
            if command.contains(['\n', '\r']) {
                bail!("setup commands must fit on one line");
            }
        }
        Ok(())
    }

    fn render(&self) -> String {
        let mut lines = vec!["[wt]".to_owned()];
        push(&mut lines, "base", self.base.as_deref());
        push(
            &mut lines,
            "env",
            self.env.as_deref().and_then(Path::to_str),
        );
        for path in &self.copies {
            push(&mut lines, "copy", path.to_str());
        }
        if self.compose {
            lines.push("\tcompose = true".to_owned());
        }
        for port in &self.ports {
            push(&mut lines, "port", Some(port));
        }
        push(&mut lines, "bootstrap", self.bootstrap.as_deref());
        push(&mut lines, "teardown", self.teardown.as_deref());
        for path in &self.disposable {
            push(&mut lines, "disposable", path.to_str());
        }
        lines.push(String::new());
        lines.join("\n")
    }

    fn validate_paths(&self) -> Result<()> {
        for path in self.copied_files().iter().chain(&self.disposable) {
            if path.as_os_str().is_empty()
                || path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                bail!(
                    "configured paths must stay inside the repository: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

pub fn parse_port_spec(value: &str) -> Result<PortSpec> {
    let (key, default) = value
        .split_once(':')
        .map_or((value, None), |(key, value)| (key, Some(value)));
    if key.is_empty()
        || !key.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphanumeric() && (index > 0 || !character.is_ascii_digit())
        })
    {
        bail!("invalid port variable: {value}");
    }
    let default = default
        .map(|port| {
            port.parse::<u16>()
                .context("port default must be a number from 1 to 65535")
        })
        .transpose()?;
    if default == Some(0) {
        bail!("port default must be a number from 1 to 65535");
    }
    Ok(PortSpec {
        key: key.to_owned(),
        default,
    })
}

fn push(lines: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        lines.push(format!("\t{key} = {}", quote(value)));
    }
}

fn quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_alphanumeric() || "._/-".contains(character))
    {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn entries(path: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(path)
        .args(["--null", "--list"])
        .output()
        .context("run git config")?;
    if !output.status.success() {
        bail!(
            "invalid {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)?
        .split_terminator('\0')
        .map(|entry| {
            let (key, value) = entry.split_once('\n').context("parse git config output")?;
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn one(path: &Path, key: &str) -> Result<Option<String>> {
    Ok(all(path, key)?.into_iter().next())
}

fn all(path: &Path, key: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(path)
        .args(["--get-all", key])
        .output()
        .context("run git config")?;
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    if !output.status.success() {
        bail!(
            "invalid {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}

fn is_true(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1"
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Config;

    #[test]
    fn loads_repeated_and_quoted_values_with_first_scalar_wins() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join(".wtconfig"),
            "[WT]\n\tBase = first\n\tbase = second\n\tEnv = \"env files/local\"\n\tCopy = one\n\tcopy = \"two files\"\n\tCompose = no\n\tcompose = yes\n\tPort = API_PORT\n\tport = WEB_PORT:3000\n\tUnknown = ignored\n",
        )
        .unwrap();

        let config = Config::load(directory.path()).unwrap();

        assert_eq!(config.base.as_deref(), Some("first"));
        assert_eq!(config.env.unwrap().to_str(), Some("env files/local"));
        assert_eq!(config.copies, ["one", "two files"].map(PathBuf::from));
        assert!(!config.compose);
        assert_eq!(config.ports, ["API_PORT", "WEB_PORT:3000"]);
    }
}
