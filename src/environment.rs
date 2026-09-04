use std::{
    collections::{BTreeMap, HashSet},
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::config::Config;

pub struct Prepared {
    pub ports: BTreeMap<String, u16>,
    pub copied_files: BTreeMap<PathBuf, String>,
}

pub fn prepare(
    config: &Config,
    source: &Path,
    target: &Path,
    compose_name: &str,
    used_ports: &HashSet<u16>,
) -> Result<Prepared> {
    reject_tracked_paths(config, target)?;
    let copied = copy_files(config, source, target)?;
    let assignments = assign_ports(config, source, used_ports)?;
    for path in &copied {
        rewrite_file(&target.join(path), &assignments)?;
    }
    if config.compose {
        let env = config.env.as_ref().context("wt.compose requires wt.env")?;
        set_value(&target.join(env), "COMPOSE_PROJECT_NAME", compose_name)?;
    }
    let copied_files = fingerprints(target, &copied)?;
    let ports = assignments
        .into_iter()
        .map(|(key, _, assigned)| (key, assigned))
        .collect();
    Ok(Prepared {
        ports,
        copied_files,
    })
}

fn reject_tracked_paths(config: &Config, target: &Path) -> Result<()> {
    for path in config.copied_files().iter().chain(&config.disposable) {
        let output = Command::new("git")
            .current_dir(target)
            .args(["ls-files", "-z", "--"])
            .arg(path)
            .output()
            .context("check configured path with git")?;
        if !output.status.success() {
            bail!("cannot inspect configured path {}", path.display());
        }
        if !output.stdout.is_empty() {
            bail!(
                "configured path is tracked by Git and must not be copied or discarded: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn copy_files(config: &Config, source: &Path, target: &Path) -> Result<Vec<PathBuf>> {
    let paths = config.copied_files();
    for relative in &paths {
        let from = source.join(relative);
        let metadata = fs::symlink_metadata(&from)
            .with_context(|| format!("copy source does not exist: {}", relative.display()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("copy source must be a regular file: {}", relative.display());
        }
        let destination = target.join(relative);
        fs::create_dir_all(
            destination
                .parent()
                .context("copy destination has no parent")?,
        )?;
        fs::copy(&from, &destination).with_context(|| format!("copy {}", relative.display()))?;
    }
    Ok(paths)
}

fn assign_ports(
    config: &Config,
    source: &Path,
    used_ports: &HashSet<u16>,
) -> Result<Vec<(String, u16, u16)>> {
    let Some(env_path) = &config.env else {
        if config.ports.is_empty() {
            return Ok(Vec::new());
        }
        bail!("wt.port requires wt.env");
    };
    let contents = fs::read_to_string(source.join(env_path))?;
    let mut unavailable = used_ports.clone();
    let mut original_ports = HashSet::new();
    let mut result = Vec::new();
    for key in &config.ports {
        let original = value(&contents, key)
            .with_context(|| format!("{key} is missing from {}", env_path.display()))?
            .parse::<u16>()
            .with_context(|| format!("{key} must be a port number"))?;
        if !original_ports.insert(original) {
            bail!("configured port values must be unique; {original} is repeated");
        }
        let assigned = available_port(original, &unavailable)?;
        unavailable.insert(assigned);
        result.push((key.clone(), original, assigned));
    }
    Ok(result)
}

fn available_port(start: u16, unavailable: &HashSet<u16>) -> Result<u16> {
    for candidate in start..=u16::MAX {
        if !unavailable.contains(&candidate) && TcpListener::bind(("127.0.0.1", candidate)).is_ok()
        {
            return Ok(candidate);
        }
    }
    bail!("no free port at or above {start}")
}

fn rewrite_file(path: &Path, assignments: &[(String, u16, u16)]) -> Result<()> {
    let mut contents = fs::read_to_string(path)?;
    for (key, _, assigned) in assignments {
        contents = replace_value(&contents, key, &assigned.to_string());
    }
    contents = replace_local_ports(contents, assignments)?;
    fs::write(path, contents).with_context(|| format!("update {}", path.display()))
}

fn replace_local_ports(mut contents: String, assignments: &[(String, u16, u16)]) -> Result<String> {
    for (index, (_, original, _)) in assignments.iter().enumerate() {
        let marker = format!("__WT_ASSIGNED_PORT_{index}__");
        if contents.contains(&marker) {
            bail!("env file contains reserved marker {marker}");
        }
        contents = contents.replace(
            &format!("localhost:{original}"),
            &format!("localhost:{marker}"),
        );
        contents = contents.replace(
            &format!("127.0.0.1:{original}"),
            &format!("127.0.0.1:{marker}"),
        );
    }
    for (index, (_, _, assigned)) in assignments.iter().enumerate() {
        contents = contents.replace(
            &format!("__WT_ASSIGNED_PORT_{index}__"),
            &assigned.to_string(),
        );
    }
    Ok(contents)
}

fn value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents.lines().find_map(|line| {
        let line = line
            .trim_start()
            .strip_prefix("export ")
            .unwrap_or(line.trim_start());
        line.strip_prefix(key)?
            .strip_prefix('=')
            .map(|value| value.trim())
    })
}

fn replace_value(contents: &str, key: &str, new_value: &str) -> String {
    let prefix = format!("{key}=");
    contents
        .lines()
        .map(|line| {
            line.strip_prefix(&prefix)
                .map_or_else(|| line.to_owned(), |_| format!("{prefix}{new_value}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if contents.ends_with('\n') { "\n" } else { "" }
}

fn set_value(path: &Path, key: &str, new_value: &str) -> Result<()> {
    let mut updated = fs::read_to_string(path)?;
    if value(&updated, key).is_some() {
        updated = replace_value(&updated, key, new_value);
    } else {
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&format!("{key}={new_value}\n"));
    }
    fs::write(path, updated)?;
    Ok(())
}

fn fingerprints(target: &Path, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, String>> {
    paths
        .iter()
        .map(|path| Ok((path.clone(), fingerprint(target, path)?)))
        .collect()
}

pub fn fingerprint(root: &Path, path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(root.join(path))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("changed managed file: {}", path.display());
    }
    let output = Command::new("git")
        .current_dir(root)
        .args(["hash-object", "--no-filters"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        bail!("cannot fingerprint {}", path.display());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn discover_ports(path: &Path) -> Result<Vec<String>> {
    let contents = fs::read_to_string(path)?;
    Ok(contents
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.ends_with("PORT") && value.trim().parse::<u16>().is_ok()).then(|| key.to_owned())
        })
        .collect())
}
