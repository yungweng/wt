use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::{
    app::repository_slug,
    detection::{git, nul_paths},
    environment,
    state::{Record, Store},
    ui::{self, style},
};

/// Paths that are generated or cached in any checkout and never worth copying.
const NOISE_DIRECTORIES: &[&str] = &[
    ".cache",
    ".devbox",
    ".direnv",
    ".git",
    ".idea",
    ".next",
    ".pytest_cache",
    ".ruff_cache",
    ".turbo",
    ".venv",
    ".vscode",
    ".wt-trash",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "tmp",
    "vendor",
];
const NOISE_FILES: &[&str] = &[".DS_Store", ".wt.env", "Thumbs.db"];
const NOISE_EXTENSIONS: &[&str] = &[
    "bak",
    "html",
    "log",
    "orig",
    "out",
    "pid",
    "swp",
    "tmp",
    "tsbuildinfo",
];

pub fn run() -> Result<()> {
    let worktree = PathBuf::from(git_line(Path::new("."), ["rev-parse", "--show-toplevel"])?);
    let common = PathBuf::from(git_line(&worktree, ["rev-parse", "--git-common-dir"])?);
    let checkout = fs::canonicalize(worktree.join(common))?
        .parent()
        .context("git common directory has no parent")?
        .to_path_buf();
    if fs::canonicalize(&worktree)? == checkout {
        bail!("run wt doctor inside a worktree created by wt, not in the main checkout");
    }
    let repository = repository_slug(&checkout)?;
    let store = Store::open_readonly()?;
    let record = store
        .records()?
        .into_iter()
        .filter(|record| record.repository.eq_ignore_ascii_case(&repository))
        .find(|record| same_path(&record.path, &worktree))
        .context("this worktree was not created by wt")?;
    ui::heading(
        repository.rsplit('/').next().unwrap_or(&repository),
        &record.branch,
    );

    let mut findings = Vec::new();
    check_private_files(&checkout, &worktree, &record, &mut findings)?;
    check_direnv(&worktree, &mut findings)?;
    check_shadowed_ports(&worktree, &record, &mut findings)?;

    if findings.is_empty() {
        println!("No problems found.");
        return Ok(());
    }
    for finding in &findings {
        println!("{} {}", style("!", 33), finding.title);
        for line in &finding.details {
            println!("    {line}");
        }
        if let Some(hint) = &finding.hint {
            println!("    {}", style(hint, 2));
        }
    }
    bail!("{} problem(s) found", findings.len())
}

struct Finding {
    title: String,
    details: Vec<String>,
    hint: Option<String>,
}

/// Ignored files of the main checkout that this worktree lacks, minus
/// generated paths. They are usually local configuration that `.wtconfig`
/// should copy.
fn check_private_files(
    checkout: &Path,
    worktree: &Path,
    record: &Record,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let output = git(
        checkout,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )
    .context("list ignored files in the main checkout")?;
    let mut missing = Vec::new();
    for entry in nul_paths(&output.stdout)? {
        let text = entry.display().to_string();
        let directory = text.ends_with('/');
        let path = Path::new(text.trim_end_matches('/'));
        if is_noise(path) || is_disposable(path, record) {
            continue;
        }
        // Wholly ignored directories are mostly build output or uploads; only
        // names that suggest configuration, such as certificates, are shown.
        if directory && !looks_like_configuration(path) {
            continue;
        }
        if !directory && is_build_output(&checkout.join(path)) {
            continue;
        }
        if fs::symlink_metadata(worktree.join(path)).is_ok() {
            continue;
        }
        missing.push(text);
    }
    if !missing.is_empty() {
        findings.push(Finding {
            title: format!(
                "{} ignored file(s) of the main checkout are missing here",
                missing.len()
            ),
            details: missing,
            hint: Some("add `copy = <path>` to .wtconfig or copy them by hand".to_owned()),
        });
    }
    let lost = record
        .copied_files
        .keys()
        .filter(|path| fs::symlink_metadata(worktree.join(path)).is_err())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !lost.is_empty() {
        findings.push(Finding {
            title: format!(
                "{} managed file(s) were deleted from this worktree",
                lost.len()
            ),
            details: lost,
            hint: Some("copy them again from the main checkout".to_owned()),
        });
    }
    Ok(())
}

fn is_noise(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| NOISE_DIRECTORIES.contains(component))
    {
        return true;
    }
    let Some(name) = components.last() else {
        return true;
    };
    if NOISE_FILES.contains(name) || name.starts_with(".cache") || name.starts_with("coverage") {
        return true;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| NOISE_EXTENSIONS.contains(&extension))
}

fn looks_like_configuration(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        "cert",
        "ssl",
        "tls",
        "key",
        "secret",
        "credential",
        "config",
        "env",
    ]
    .iter()
    .any(|hint| name.contains(hint))
}

/// Executables without an extension are compiled binaries, not configuration.
fn is_build_output(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.extension().is_none()
            && fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn is_disposable(path: &Path, record: &Record) -> bool {
    record
        .disposable
        .iter()
        .any(|disposable| path == disposable || path.starts_with(disposable))
}

fn check_direnv(worktree: &Path, findings: &mut Vec<Finding>) -> Result<()> {
    if !worktree.join(".envrc").is_file() {
        return Ok(());
    }
    if environment::direnv_allowed(worktree)? == Some(false) {
        findings.push(Finding {
            title: "direnv has not allowed .envrc, so leased ports in .wt.env are not loaded"
                .to_owned(),
            details: Vec::new(),
            hint: Some("run: direnv allow".to_owned()),
        });
    }
    Ok(())
}

/// Ports leased for this worktree that the shell or `.envrc` overrides. Docker
/// Compose and most tools prefer the environment over env files, so a
/// user-wide value silently points the worktree at another checkout's ports.
fn check_shadowed_ports(
    worktree: &Path,
    record: &Record,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    if record.ports.is_empty() {
        return Ok(());
    }
    let mut shadowed = Vec::new();
    for (key, assigned) in &record.ports {
        if let Ok(actual) = std::env::var(key) {
            if actual != assigned.to_string() {
                shadowed.push(format!(
                    "{key}={actual} in this shell (worktree: {assigned})"
                ));
            }
        }
    }
    if worktree.join(".envrc").is_file() && environment::direnv_allowed(worktree)? == Some(true) {
        for (key, value) in direnv_exports(worktree)? {
            if let Some(assigned) = record.ports.get(&key) {
                if value != assigned.to_string() {
                    shadowed.push(format!("{key}={value} from .envrc (worktree: {assigned})"));
                }
            }
        }
    }
    if !shadowed.is_empty() {
        findings.push(Finding {
            title: format!(
                "{} leased port(s) are shadowed by the environment",
                shadowed.len()
            ),
            details: shadowed,
            hint: Some(
                "remove port values from user-wide env files, or load .wt.env last in .envrc"
                    .to_owned(),
            ),
        });
    }
    Ok(())
}

/// Variables `.envrc` exports, evaluated from a clean direnv state.
fn direnv_exports(worktree: &Path) -> Result<BTreeMap<String, String>> {
    let mut command = Command::new("direnv");
    command.current_dir(worktree).args(["export", "json"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("DIRENV_") {
            command.env_remove(key);
        }
    }
    let Ok(output) = command.output() else {
        return Ok(BTreeMap::new());
    };
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(BTreeMap::new());
    }
    let exports: BTreeMap<String, Option<String>> =
        serde_json::from_slice(&output.stdout).context("parse direnv export")?;
    Ok(exports
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?)))
        .collect())
}

fn same_path(recorded: &Path, current: &Path) -> bool {
    match (fs::canonicalize(recorded), fs::canonicalize(current)) {
        (Ok(recorded), Ok(current)) => recorded == current,
        _ => recorded == current,
    }
}

fn git_line<const N: usize>(directory: &Path, args: [&str; N]) -> Result<String> {
    let output = git(directory, args)?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
