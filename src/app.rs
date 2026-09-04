use std::{
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    config::{self, Config},
    environment,
    state::{Record, Store},
};

#[derive(Deserialize)]
struct Repository {
    #[serde(rename = "nameWithOwner")]
    slug: String,
    #[serde(rename = "defaultBranchRef")]
    default_branch: Branch,
}

#[derive(Deserialize)]
struct Branch {
    name: String,
}

#[derive(Deserialize)]
struct Issue {
    number: u64,
    title: String,
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

struct AddPlan {
    issue: u64,
    branch: String,
    path: PathBuf,
    compose_name: String,
}

struct EnvironmentAnswers {
    env: Option<PathBuf>,
    copies: Vec<PathBuf>,
    ports: Vec<String>,
}

pub fn add(reference: &str, no_bootstrap: bool) -> Result<()> {
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository(&repo_root)?;
    let number = issue_number(reference, &repository.slug)?;
    let store = Store::open()?;
    let _lock = store.lock()?;
    if let Some(record) = store.find(&repository.slug, number)?
        && record.path.exists()
    {
        println!("{}", record.path.display());
        return Ok(());
    }
    offer_setup(&repo_root, &repository)?;
    let issue = progress("Reading GitHub issue", "Issue loaded", || {
        issue(&repo_root, number, &repository.slug)
    })?;
    let plan = add_plan(&repository, &issue)?;
    let config = Config::load(&repo_root)?;
    let config_hash = config_hash(&repo_root)?;
    if !no_bootstrap {
        ensure_trusted_bootstrap(&store, &repository.slug, &config, config_hash.as_deref())?;
    }
    progress("Creating worktree", "Worktree ready", || {
        install_worktree(&store, &config, &repo_root, &repository, &plan, config_hash)
    })?;
    run_bootstrap(&config, &plan, no_bootstrap)?;
    if std::io::stderr().is_terminal() {
        cliclack::log::success(format!("Ready at {}", plan.path.display()))?;
    }
    println!("{}", plan.path.display());
    Ok(())
}

fn add_plan(repository: &Repository, issue: &Issue) -> Result<AddPlan> {
    let branch = branch_name(issue);
    let directory = branch.replace('/', "-");
    let repo_name = repository.slug.rsplit('/').next().unwrap_or("repository");
    Ok(AddPlan {
        issue: issue.number,
        path: config::worktree_root()?.join(repo_name).join(&directory),
        compose_name: compose_name(&repository.slug, issue.number),
        branch,
    })
}

fn compose_name(repository: &str, issue: u64) -> String {
    let raw = format!("wt-{issue}-{repository}");
    let mut name = String::new();
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
        } else if !name.ends_with('-') {
            name.push('-');
        }
        if name.len() == 63 {
            break;
        }
    }
    name.trim_end_matches('-').to_owned()
}

fn install_worktree(
    store: &Store,
    config: &Config,
    repo_root: &Path,
    repository: &Repository,
    plan: &AddPlan,
    config_hash: Option<String>,
) -> Result<()> {
    let base = config
        .base
        .as_deref()
        .unwrap_or(&repository.default_branch.name);
    run_git(repo_root, ["fetch", "origin", base])?;
    let start = format!("origin/{base}");
    create_worktree(repo_root, &plan.path, &plan.branch, &start)?;
    let setup = prepare_record(
        store,
        config,
        repo_root,
        plan,
        &repository.slug,
        config_hash,
    );
    if let Err(error) = setup {
        let _ = run_git(
            repo_root,
            ["worktree", "remove", "--force", path_str(&plan.path)?],
        );
        return Err(error);
    }
    Ok(())
}

fn prepare_record(
    store: &Store,
    config: &Config,
    repo_root: &Path,
    plan: &AddPlan,
    repository: &str,
    config_hash: Option<String>,
) -> Result<()> {
    let prepared = environment::prepare(
        config,
        repo_root,
        &plan.path,
        &plan.compose_name,
        &store.used_ports()?,
    )?;
    store.save(&Record {
        repository: repository.to_owned(),
        issue: plan.issue,
        branch: plan.branch.clone(),
        path: plan.path.clone(),
        ports: prepared.ports,
        copied_files: prepared.copied_files,
        teardown: config.teardown.clone(),
        disposable: config.disposable.clone(),
        config_hash,
    })
}

fn run_bootstrap(config: &Config, plan: &AddPlan, skipped: bool) -> Result<()> {
    if let Some(command) = &config.bootstrap
        && !skipped
    {
        progress("Running bootstrap", "Bootstrap complete", || {
            run_hook("bootstrap", command, &plan.path)
        })
        .with_context(|| format!("bootstrap failed; worktree kept at {}", plan.path.display()))?;
    }
    Ok(())
}

fn create_worktree(repo: &Path, path: &Path, branch: &str, start: &str) -> Result<()> {
    let reference = format!("refs/heads/{branch}");
    let exists = Command::new("git")
        .current_dir(repo)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .status()?
        .success();
    if exists {
        run_git(repo, ["worktree", "add", path_str(path)?, branch])
    } else {
        run_git(
            repo,
            ["worktree", "add", "-b", branch, path_str(path)?, start],
        )
    }
}

pub struct InitOptions {
    pub root: Option<String>,
    pub base: Option<String>,
    pub env: Option<String>,
    pub copies: Vec<String>,
    pub compose: bool,
    pub ports: Vec<String>,
    pub bootstrap: Option<String>,
    pub teardown: Option<String>,
    pub disposable: Vec<String>,
    pub yes: bool,
}

pub fn init(mut options: InitOptions) -> Result<()> {
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository(&repo_root)?;
    configure_worktree_root(&mut options)?;
    let config = if options.yes {
        config_from_options(options, &repository)
    } else {
        interactive_config(options, &repo_root, &repository)?
    };
    config.validate_for_write()?;
    config.write(&repo_root)?;
    let hash = config_hash(&repo_root)?.context("fingerprint .wtconfig")?;
    Store::open()?.trust(&repository.slug, &hash)?;
    if std::io::stderr().is_terminal() {
        cliclack::outro("Saved and trusted .wtconfig")?;
    }
    Ok(())
}

fn configure_worktree_root(options: &mut InitOptions) -> Result<()> {
    if options.yes {
        if let Some(root) = options.root.take() {
            config::write_worktree_root(Path::new(&root))?;
        }
        return Ok(());
    }
    let default = config::worktree_root()?;
    let root: String = cliclack::input("Where should worktrees live?")
        .default_input(&default.display().to_string())
        .interact()?;
    config::write_worktree_root(Path::new(&root))
}

pub fn trust(yes: bool) -> Result<()> {
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository(&repo_root)?;
    let config = Config::load(&repo_root)?;
    let hash = config_hash(&repo_root)?.context("no .wtconfig to trust")?;
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("trust confirmation needs a terminal; review the file and pass --yes");
        }
        cliclack::intro(" wt trust ")?;
        let commands = format!(
            "bootstrap: {}\nteardown: {}",
            config.bootstrap.as_deref().unwrap_or("(none)"),
            config.teardown.as_deref().unwrap_or("(none)")
        );
        cliclack::note(repo_root.join(".wtconfig").display(), commands)?;
        if !cliclack::confirm("Trust this exact configuration?")
            .initial_value(false)
            .interact()?
        {
            bail!("configuration was not trusted");
        }
    }
    Store::open()?.trust(&repository.slug, &hash)?;
    if std::io::stderr().is_terminal() {
        cliclack::outro("Configuration trusted")?;
    }
    Ok(())
}

pub fn list(porcelain: bool, all: bool) -> Result<()> {
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository(&repo_root)?;
    let mut records = Store::open()?.records()?;
    if !all {
        records.retain(|record| record.repository == repository.slug);
    }
    records.sort_by_key(|record| (record.repository.clone(), record.issue));
    if !porcelain && !records.is_empty() {
        println!("ISSUE\tBRANCH\tPATH");
    }
    for record in records {
        println!(
            "{}\t{}\t{}",
            record.issue,
            record.branch,
            record.path.display()
        );
    }
    Ok(())
}

pub fn remove(issue: u64, force: bool, skip_teardown: bool) -> Result<()> {
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository(&repo_root)?;
    let store = Store::open()?;
    let _lock = store.lock()?;
    let record = store
        .find(&repository.slug, issue)?
        .with_context(|| format!("no managed worktree for issue {issue}"))?;
    if !force {
        ensure_safe_to_remove(&record)?;
    }
    if !skip_teardown {
        run_recorded_teardown(&store, &record)?;
    }
    progress("Removing worktree", "Worktree removed", || {
        remove_recorded_worktree(&repo_root, &record, force)
    })?;
    store.delete(&repository.slug, issue)?;
    if std::io::stderr().is_terminal() {
        cliclack::log::success(format!("Kept branch {}", record.branch))?;
    }
    Ok(())
}

fn remove_recorded_worktree(repo: &Path, record: &Record, force: bool) -> Result<()> {
    if force {
        return run_git(
            repo,
            ["worktree", "remove", "--force", path_str(&record.path)?],
        );
    }
    delete_managed_files(record)?;
    run_git(repo, ["worktree", "remove", path_str(&record.path)?])
}

fn config_from_options(options: InitOptions, repository: &Repository) -> Config {
    Config {
        base: Some(
            options
                .base
                .unwrap_or_else(|| repository.default_branch.name.clone()),
        ),
        env: options.env.map(PathBuf::from),
        copies: options.copies.into_iter().map(PathBuf::from).collect(),
        compose: options.compose,
        ports: options.ports,
        bootstrap: options.bootstrap.filter(|value| !value.is_empty()),
        teardown: options.teardown.filter(|value| !value.is_empty()),
        disposable: options.disposable.into_iter().map(PathBuf::from).collect(),
    }
}

fn interactive_config(
    options: InitOptions,
    repo: &Path,
    repository: &Repository,
) -> Result<Config> {
    if !std::io::stdin().is_terminal() {
        bail!("interactive setup needs a terminal; pass setup options with --yes");
    }
    cliclack::intro(" wt setup ")?;
    let default_base = options
        .base
        .as_deref()
        .unwrap_or(&repository.default_branch.name);
    let base = cliclack::input("Base branch?")
        .default_input(default_base)
        .interact()?;
    let environment = prompt_environment(repo, options.env, options.ports, options.copies)?;
    let compose = cliclack::confirm("Use an isolated Docker Compose project?")
        .initial_value(options.compose || has_compose_file(repo))
        .interact()?;
    let bootstrap = prompt_optional("Bootstrap command?", options.bootstrap)?;
    let teardown = prompt_optional("Teardown command?", options.teardown)?;
    let disposable = prompt_list(
        "Disposable paths? Comma-separated",
        options.disposable,
        Vec::new(),
    )?;
    Ok(Config {
        base: Some(base),
        env: environment.env,
        copies: environment.copies,
        compose,
        ports: environment.ports,
        bootstrap,
        teardown,
        disposable: disposable.into_iter().map(PathBuf::from).collect(),
    })
}

fn prompt_environment(
    repo: &Path,
    supplied_env: Option<String>,
    supplied_ports: Vec<String>,
    supplied_copies: Vec<String>,
) -> Result<EnvironmentAnswers> {
    let candidates = ignored_env_files(repo)?;
    let detected_env =
        supplied_env.or_else(|| candidates.first().map(|path| path.display().to_string()));
    let env: String = cliclack::input("Primary env file? Leave empty for none")
        .default_input(detected_env.as_deref().unwrap_or(""))
        .required(false)
        .interact()?;
    let detected_ports = if env.is_empty() {
        Vec::new()
    } else {
        environment::discover_ports(&repo.join(&env)).unwrap_or_default()
    };
    let detected_copies = candidates
        .into_iter()
        .map(|path| path.display().to_string())
        .filter(|path| path != &env)
        .collect();
    Ok(EnvironmentAnswers {
        env: (!env.is_empty()).then(|| PathBuf::from(&env)),
        ports: prompt_list(
            "Port variables? Comma-separated",
            supplied_ports,
            detected_ports,
        )?,
        copies: prompt_list(
            "Other files to copy? Comma-separated",
            supplied_copies,
            detected_copies,
        )?
        .into_iter()
        .map(PathBuf::from)
        .collect(),
    })
}

fn prompt_list(label: &str, supplied: Vec<String>, detected: Vec<String>) -> Result<Vec<String>> {
    let defaults = if supplied.is_empty() {
        detected
    } else {
        supplied
    };
    let input: String = cliclack::input(label)
        .default_input(&defaults.join(", "))
        .required(false)
        .interact()?;
    Ok(input
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect())
}

fn prompt_optional(label: &str, supplied: Option<String>) -> Result<Option<String>> {
    let input: String = cliclack::input(label)
        .default_input(supplied.as_deref().unwrap_or(""))
        .required(false)
        .interact()?;
    Ok((!input.trim().is_empty()).then(|| input.trim().to_owned()))
}

fn has_compose_file(repo: &Path) -> bool {
    [
        "compose.yml",
        "compose.yaml",
        "docker-compose.yml",
        "docker-compose.yaml",
    ]
    .iter()
    .any(|file| repo.join(file).is_file())
}

fn ignored_env_files(repo: &Path) -> Result<Vec<PathBuf>> {
    let output = run(Command::new("git").current_dir(repo).args([
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "-z",
    ]))?;
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8(entry.to_vec()).map(PathBuf::from))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".env") && !name.ends_with(".example"))
    });
    paths.sort_by_key(|path| path != Path::new(".env"));
    Ok(paths)
}

fn offer_setup(repo: &Path, repository: &Repository) -> Result<()> {
    if repo.join(".wtconfig").exists() || !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let configure = cliclack::confirm("No .wtconfig found. Configure this repository?")
        .initial_value(true)
        .interact()?;
    if configure {
        init(InitOptions {
            root: None,
            base: Some(repository.default_branch.name.clone()),
            env: None,
            copies: Vec::new(),
            compose: false,
            ports: Vec::new(),
            bootstrap: None,
            teardown: None,
            disposable: Vec::new(),
            yes: false,
        })?;
    }
    Ok(())
}

fn config_hash(repo: &Path) -> Result<Option<String>> {
    if !repo.join(".wtconfig").exists() {
        return Ok(None);
    }
    Ok(Some(environment::fingerprint(
        repo,
        Path::new(".wtconfig"),
    )?))
}

fn ensure_trusted_bootstrap(
    store: &Store,
    repository: &str,
    config: &Config,
    hash: Option<&str>,
) -> Result<()> {
    if config.bootstrap.is_none() {
        return Ok(());
    }
    let hash = hash.context("configuration commands require .wtconfig")?;
    if store.is_trusted(repository, hash)? {
        return Ok(());
    }
    bail!(".wtconfig commands are not trusted; review them and run wt trust")
}

fn run_hook(name: &str, command: &str, directory: &Path) -> Result<()> {
    let output = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(directory)
        .output()
        .with_context(|| format!("start {name} command"))?;
    if !output.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.status.success() {
        bail!(
            "{name} command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

fn run_recorded_teardown(store: &Store, record: &Record) -> Result<()> {
    let Some(command) = &record.teardown else {
        return Ok(());
    };
    let hash = record
        .config_hash
        .as_deref()
        .context("teardown command has no trust record")?;
    if !store.is_trusted(&record.repository, hash)? {
        bail!("teardown command is not trusted; use --skip-teardown to leave it running");
    }
    run_hook("teardown", command, &record.path)
}

fn progress<T>(message: &str, completed: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    if !std::io::stderr().is_terminal() {
        return operation();
    }
    let spinner = cliclack::spinner();
    spinner.start(message);
    match operation() {
        Ok(value) => {
            spinner.stop(completed);
            Ok(value)
        }
        Err(error) => {
            spinner.stop("Failed");
            Err(error)
        }
    }
}

fn ensure_safe_to_remove(record: &Record) -> Result<()> {
    for (path, expected) in &record.copied_files {
        let actual = environment::fingerprint(&record.path, path)?;
        if &actual != expected {
            bail!("changed managed file: {}", path.display());
        }
    }
    for (kind, path) in worktree_changes(&record.path)? {
        if kind != "??" && kind != "!!" {
            bail!("worktree contains tracked changes: {path}");
        }
        if !is_removable_path(Path::new(&path), record) {
            bail!("worktree contains an unmanaged file: {path}");
        }
    }
    Ok(())
}

fn worktree_changes(path: &Path) -> Result<Vec<(String, String)>> {
    let output = run(Command::new("git").current_dir(path).args([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignored=matching",
    ]))?;
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(parse_status_entry)
        .collect()
}

fn parse_status_entry(entry: &[u8]) -> Result<(String, String)> {
    if entry.len() < 4 {
        bail!("unexpected git status output");
    }
    Ok((
        String::from_utf8(entry[..2].to_vec())?,
        String::from_utf8(entry[3..].to_vec())?,
    ))
}

fn is_removable_path(path: &Path, record: &Record) -> bool {
    record.copied_files.contains_key(path)
        || record
            .disposable
            .iter()
            .any(|disposable| path == disposable || path.starts_with(disposable))
}

fn delete_managed_files(record: &Record) -> Result<()> {
    for path in record.copied_files.keys() {
        fs::remove_file(record.path.join(path))?;
    }
    for path in &record.disposable {
        let path = record.path.join(path);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(path)?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

fn repository(repo_root: &Path) -> Result<Repository> {
    let output = run(Command::new("gh").current_dir(repo_root).args([
        "repo",
        "view",
        "--json",
        "nameWithOwner,defaultBranchRef",
    ]))?;
    serde_json::from_slice(&output.stdout).context("parse repository details from gh")
}

fn issue(repo_root: &Path, number: u64, slug: &str) -> Result<Issue> {
    let output = run(Command::new("gh").current_dir(repo_root).args([
        "issue",
        "view",
        &number.to_string(),
        "--repo",
        slug,
        "--json",
        "number,title,state,labels",
    ]))?;
    serde_json::from_slice(&output.stdout).context("parse issue details from gh")
}

fn issue_number(reference: &str, slug: &str) -> Result<u64> {
    if let Ok(number) = reference.parse() {
        return Ok(number);
    }
    let prefix = format!("https://github.com/{slug}/issues/");
    let Some(number) = reference.strip_prefix(&prefix) else {
        bail!("expected an issue number or a {prefix}<number> URL");
    };
    number.parse().context("parse issue number from URL")
}

fn branch_name(issue: &Issue) -> String {
    let kind = issue
        .labels
        .iter()
        .map(|label| label.name.to_ascii_lowercase())
        .find_map(|label| branch_kind(&label))
        .unwrap_or("work");
    format!("{kind}/{}-{}", issue.number, slugify(&issue.title))
}

fn branch_kind(label: &str) -> Option<&'static str> {
    match label {
        "bug" | "type: bug" | "type/bug" => Some("fix"),
        "feature" | "enhancement" | "type: feature" => Some("feat"),
        "documentation" | "docs" => Some("docs"),
        _ => None,
    }
}

fn slugify(title: &str) -> String {
    let mut slug = String::new();
    for character in title.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            slug.push(character);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "issue".to_owned()
    } else {
        slug.to_owned()
    }
}

fn git_output<const N: usize>(args: [&str; N]) -> Result<String> {
    let output = run(Command::new("git").args(args))?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn run_git<const N: usize>(dir: &Path, args: [&str; N]) -> Result<()> {
    run(Command::new("git").current_dir(dir).args(args))?;
    Ok(())
}

fn run(command: &mut Command) -> Result<Output> {
    let output = command.output().context("start command")?;
    if output.status.success() {
        return Ok(output);
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!("command failed: {message}")
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str().context("path is not valid UTF-8")
}
