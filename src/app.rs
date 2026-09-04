use std::{
    fs,
    io::{self, IsTerminal, Seek},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    config::{self, Config},
    detection::{self, EnvTemplate, Report},
    environment,
    state::{Record, Store},
    ui::{self, progress, style},
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
    issue: Option<u64>,
    branch: String,
    path: PathBuf,
    compose_name: String,
}

struct EnvironmentAnswers {
    env: Option<PathBuf>,
    copies: Vec<PathBuf>,
    ports: Vec<String>,
}

enum AddTarget {
    Issue(u64),
    Branch(String),
}

pub fn add(reference: &str, no_bootstrap: bool, verbose: bool) -> Result<()> {
    let started = Instant::now();
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let slug = repository_slug(&repo_root)?;
    let target = add_target(reference, &slug)?;
    let store = Store::open()?;
    let target_key = match &target {
        AddTarget::Issue(number) => number.to_string(),
        AddTarget::Branch(branch) => branch.clone(),
    };
    // Always take the worktree lock before the shared port/state lock.
    let _worktree_lock = store.lock_worktree(&slug, &target_key)?;
    let lock = store.lock()?;
    if let Some(record) = find_record(&store, &slug, &target)? {
        if record.path.exists() {
            println!("{}", record.path.display());
            return Ok(());
        }
    }
    ui::heading(slug.rsplit('/').next().unwrap_or(&slug), &target_key);
    offer_setup(&repo_root)?;
    let plan = target_plan(&repo_root, &slug, target)?;
    let config = Config::load(&repo_root)?;
    let base = base_branch(&repo_root, &config, plan.issue)?;
    let config_hash = config.command_fingerprint()?;
    ensure_trusted_commands(&store, &slug, &config, config_hash.as_deref(), no_bootstrap)?;
    progress("Creating worktree", "Worktree created", || {
        install_worktree(
            &store,
            &config,
            &repo_root,
            &slug,
            &base,
            &plan,
            config_hash,
        )
    })?;
    // Ports and the record are saved. Other worktrees can now finish setup.
    drop(lock);
    run_bootstrap(&config, &plan, no_bootstrap, verbose)?;
    ui::ready(started, no_bootstrap && config.bootstrap.is_some());
    println!("{}", plan.path.display());
    Ok(())
}

fn find_record(store: &Store, repository: &str, target: &AddTarget) -> Result<Option<Record>> {
    match target {
        AddTarget::Issue(number) => store.find_issue(repository, *number),
        AddTarget::Branch(branch) => store.find_branch(repository, branch),
    }
}

fn target_plan(repo: &Path, repository: &str, target: AddTarget) -> Result<AddPlan> {
    match target {
        AddTarget::Issue(number) => {
            let issue = progress("Reading GitHub issue", "Issue loaded", || {
                issue(repo, number, repository)
            })?;
            issue_plan(repository, &issue)
        }
        AddTarget::Branch(branch) => add_plan(repository, None, branch),
    }
}

fn base_branch(repo: &Path, config: &Config, issue: Option<u64>) -> Result<String> {
    if let Some(base) = &config.base {
        return Ok(base.clone());
    }
    if issue.is_some() {
        return Ok(repository(repo)?.default_branch.name);
    }
    let branch = git_output_in(repo, ["branch", "--show-current"])?;
    if branch.is_empty() {
        bail!("cannot create a branch from detached HEAD; configure wt.base");
    }
    Ok(branch)
}

fn issue_plan(repository: &str, issue: &Issue) -> Result<AddPlan> {
    add_plan(repository, Some(issue.number), branch_name(issue))
}

fn add_plan(repository: &str, issue: Option<u64>, branch: String) -> Result<AddPlan> {
    let directory = branch.replace('/', "-");
    let repo_name = repository.rsplit('/').next().unwrap_or("repository");
    Ok(AddPlan {
        issue,
        path: config::worktree_root()?.join(repo_name).join(&directory),
        compose_name: compose_name(
            repository,
            issue.map_or(branch.as_str().to_owned(), |n| n.to_string()),
        ),
        branch,
    })
}

fn compose_name(repository: &str, reference: String) -> String {
    let raw = format!("wt-{reference}-{repository}");
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
    repository: &str,
    base: &str,
    plan: &AddPlan,
    config_hash: Option<String>,
) -> Result<()> {
    create_worktree(repo_root, &plan.path, &plan.branch, base)?;
    let setup = prepare_record(store, config, repo_root, plan, repository, config_hash);
    if let Err(error) = setup {
        let cleanup = run_git(
            repo_root,
            ["worktree", "remove", "--force", path_str(&plan.path)?],
        );
        if let Err(cleanup) = cleanup {
            bail!("{error:#}; rollback also failed: {cleanup:#}");
        }
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

fn run_bootstrap(config: &Config, plan: &AddPlan, skipped: bool, verbose: bool) -> Result<()> {
    if !skipped {
        if let Some(command) = &config.bootstrap {
            run_hook("bootstrap", command, &plan.path, verbose).with_context(|| {
                format!("bootstrap failed; worktree kept at {}", plan.path.display())
            })?;
        }
    }
    Ok(())
}

fn create_worktree(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if local_branch_exists(repo, branch)? {
        run_git(repo, ["worktree", "add", path_str(path)?, branch])
    } else if remote_branch_exists(repo, branch)? {
        let remote = format!("origin/{branch}");
        run_git(
            repo,
            [
                "worktree",
                "add",
                "--track",
                "-b",
                branch,
                path_str(path)?,
                &remote,
            ],
        )
    } else {
        let start = starting_point(repo, base)?;
        run_git(
            repo,
            ["worktree", "add", "-b", branch, path_str(path)?, &start],
        )
    }
}

fn starting_point(repo: &Path, base: &str) -> Result<String> {
    if local_branch_exists(repo, base)? {
        return Ok(base.to_owned());
    }
    run_git(repo, ["fetch", "origin", base])?;
    Ok(format!("origin/{base}"))
}

fn remote_branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    reference_exists(repo, &format!("refs/remotes/origin/{branch}"))
}

fn local_branch_exists(repo: &Path, branch: &str) -> Result<bool> {
    reference_exists(repo, &format!("refs/heads/{branch}"))
}

fn reference_exists(repo: &Path, reference: &str) -> Result<bool> {
    Ok(Command::new("git")
        .current_dir(repo)
        .args(["show-ref", "--verify", "--quiet", reference])
        .status()?
        .success())
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
    let (config, env_template) = if options.yes {
        if let Some(root) = options.root.take() {
            config::write_worktree_root(Path::new(&root))?;
        }
        (config_from_options(options, &repository), None)
    } else {
        let (root, config, template) = interactive_config(options, &repo_root, &repository)?;
        config::write_worktree_root(&root)?;
        (config, template)
    };
    config.validate_for_write()?;
    detection::apply_changes(&repo_root, &config, env_template.as_ref())?;
    config.write(&repo_root)?;
    let hash = config.command_fingerprint()?;
    if let Some(hash) = &hash {
        Store::open()?.trust(&repository.slug, hash)?;
    }
    if std::io::stderr().is_terminal() {
        let message = if hash.is_some() {
            "Saved .wtconfig and trusted its commands"
        } else {
            "Saved .wtconfig"
        };
        cliclack::outro(message)?;
    }
    Ok(())
}

pub fn trust(yes: bool) -> Result<()> {
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository_slug(&repo_root)?;
    let config = Config::load(&repo_root)?;
    let hash = config
        .command_fingerprint()?
        .context(".wtconfig has no bootstrap or teardown commands to trust")?;
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
        if !cliclack::confirm("Trust these commands?")
            .initial_value(false)
            .interact()?
        {
            bail!("commands were not trusted");
        }
    }
    Store::open()?.trust(&repository, &hash)?;
    if std::io::stderr().is_terminal() {
        cliclack::outro("Commands trusted")?;
    }
    Ok(())
}

pub fn list(porcelain: bool, all: bool) -> Result<()> {
    let mut records = Store::open()?.records()?;
    if !all {
        let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
        let repository = repository_slug(&repo_root)?;
        records.retain(|record| record.repository.eq_ignore_ascii_case(&repository));
    }
    records.sort_by_key(|record| {
        (
            record.repository.clone(),
            record.issue.is_none(),
            record.issue.unwrap_or_default(),
            record.branch.clone(),
        )
    });
    if !porcelain && !records.is_empty() {
        println!("ISSUE\tBRANCH\tPATH");
    }
    for record in records {
        println!(
            "{}\t{}\t{}",
            record
                .issue
                .map_or_else(|| "-".to_owned(), |issue| issue.to_string()),
            record.branch,
            record.path.display()
        );
    }
    Ok(())
}

pub fn remove(reference: &str, force: bool, skip_teardown: bool, verbose: bool) -> Result<()> {
    let repo_root = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository_slug(&repo_root)?;
    let store = Store::open()?;
    let _worktree_lock = store.lock_worktree(&repository, reference)?;
    let _lock = store.lock()?;
    let record = match reference.parse::<u64>() {
        Ok(issue) => store.find_issue(&repository, issue)?,
        Err(_) => store.find_branch(&repository, reference)?,
    }
    .with_context(|| format!("no managed worktree for {reference}"))?;
    if !force {
        ensure_safe_to_remove(&record)?;
    }
    if !skip_teardown {
        run_recorded_teardown(&store, &record, verbose)?;
    }
    progress("Removing worktree", "Worktree removed", || {
        remove_recorded_worktree(&repo_root, &record, force)
    })?;
    store.delete(&record)?;
    if std::io::stderr().is_terminal() {
        eprintln!("{} Kept branch {}", style("◇", 32), record.branch);
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
) -> Result<(PathBuf, Config, Option<EnvTemplate>)> {
    if !std::io::stdin().is_terminal() {
        bail!("interactive setup needs a terminal; pass setup options with --yes");
    }
    let root = options
        .root
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(config::worktree_root()?);
    let seed = config_from_options(options, repository);
    let report = detection::detect(repo, seed)?;
    cliclack::intro(" wt setup ")?;
    let changes = detection::planned_changes(repo, &report.config, report.env_template.as_ref())?;
    cliclack::note(
        "Detected settings",
        config_summary(&root, &report, &changes),
    )?;
    let question = if report.warnings.is_empty() {
        "Use these settings? (No to customize)"
    } else {
        "Use these settings anyway? (No to customize)"
    };
    if cliclack::confirm(question)
        .initial_value(report.warnings.is_empty())
        .interact()?
    {
        return Ok((root, report.config, report.env_template));
    }
    customize_config(root, report, &repository.default_branch.name)
}

fn customize_config(
    root: PathBuf,
    detected: Report,
    default_branch: &str,
) -> Result<(PathBuf, Config, Option<EnvTemplate>)> {
    let root: String = cliclack::input("Worktree root?")
        .default_input(&root.display().to_string())
        .interact()?;
    let base = cliclack::input("Base branch?")
        .default_input(detected.config.base.as_deref().unwrap_or(default_branch))
        .interact()?;
    let environment = prompt_environment(
        detected
            .config
            .env
            .as_ref()
            .map(|path| path.display().to_string()),
        detected.config.ports.clone(),
        path_strings(detected.config.copies.clone()),
    )?;
    let compose = cliclack::confirm("Use an isolated Docker Compose project?")
        .initial_value(detected.config.compose)
        .interact()?;
    let bootstrap = prompt_optional("Bootstrap command?", detected.config.bootstrap.clone())?;
    let teardown = prompt_optional("Teardown command?", detected.config.teardown.clone())?;
    let disposable = prompt_list(
        "Disposable paths? Comma-separated",
        path_strings(detected.config.disposable.clone()),
        Vec::new(),
    )?;
    let config = custom_config(base, environment, compose, bootstrap, teardown, disposable);
    let template = detected
        .env_template
        .filter(|template| config.env.as_ref() == Some(&template.target));
    Ok((PathBuf::from(root), config, template))
}

fn custom_config(
    base: String,
    environment: EnvironmentAnswers,
    compose: bool,
    bootstrap: Option<String>,
    teardown: Option<String>,
    disposable: Vec<String>,
) -> Config {
    Config {
        base: Some(base),
        env: environment.env,
        copies: environment.copies,
        compose,
        ports: environment.ports,
        bootstrap,
        teardown,
        disposable: disposable.into_iter().map(PathBuf::from).collect(),
    }
}

fn path_strings(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.display().to_string())
        .collect()
}

fn config_summary(root: &Path, report: &Report, changes: &[String]) -> String {
    let value = |value: Option<&str>, fallback: &str| value.unwrap_or(fallback).to_owned();
    let paths = |paths: &[PathBuf]| {
        if paths.is_empty() {
            "None".to_owned()
        } else {
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut summary = summary_fields(root, report, &paths, &value);
    append_section(&mut summary, "Info", &report.notices, "");
    append_section(
        &mut summary,
        "Repository setup",
        changes,
        "Commit these files on the base branch before creating a worktree.",
    );
    append_section(&mut summary, "Warnings", &report.warnings, "");
    summary
}

fn summary_fields(
    root: &Path,
    report: &Report,
    paths: &impl Fn(&[PathBuf]) -> String,
    value: &impl Fn(Option<&str>, &str) -> String,
) -> String {
    let config = &report.config;
    format!(
        "Worktree root  {}\nBase branch    {}\nEnvironment    {}\nOther files    {}\nDevelopment    {}\nDocker Compose {}\nBootstrap      {}\nTeardown       {}\nDisposable     {}",
        root.display(),
        value(config.base.as_deref(), "Not detected"),
        config
            .env
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Not found".to_owned()),
        paths(&config.copies),
        if report.development.is_empty() {
            "Not detected".to_owned()
        } else {
            report.development.join("\n               ")
        },
        if config.compose { "Yes" } else { "No" },
        value(config.bootstrap.as_deref(), "None"),
        value(config.teardown.as_deref(), "None"),
        paths(&config.disposable),
    )
}

fn append_section(summary: &mut String, title: &str, lines: &[String], footer: &str) {
    if lines.is_empty() {
        return;
    }
    summary.push_str(&format!("\n\n{title}\n• {}", lines.join("\n• ")));
    if !footer.is_empty() {
        summary.push_str(&format!("\n{footer}"));
    }
}

fn prompt_environment(
    supplied_env: Option<String>,
    supplied_ports: Vec<String>,
    supplied_copies: Vec<String>,
) -> Result<EnvironmentAnswers> {
    let env: String = cliclack::input("Primary env file? Leave empty for none")
        .default_input(supplied_env.as_deref().unwrap_or(""))
        .required(false)
        .interact()?;
    Ok(EnvironmentAnswers {
        env: (!env.is_empty()).then(|| PathBuf::from(&env)),
        ports: prompt_list(
            "Port variables? Comma-separated",
            supplied_ports,
            Vec::new(),
        )?,
        copies: prompt_list(
            "Other files to copy? Comma-separated",
            supplied_copies,
            Vec::new(),
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

fn offer_setup(repo: &Path) -> Result<()> {
    if repo.join(".wtconfig").exists() || !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let configure = cliclack::confirm("No .wtconfig found. Configure this repository?")
        .initial_value(true)
        .interact()?;
    if configure {
        init(InitOptions {
            root: None,
            base: None,
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

fn ensure_trusted_commands(
    store: &Store,
    repository: &str,
    config: &Config,
    hash: Option<&str>,
    no_bootstrap: bool,
) -> Result<()> {
    if (no_bootstrap || config.bootstrap.is_none()) && config.teardown.is_none() {
        return Ok(());
    }
    let hash = hash.context("configuration commands require a fingerprint")?;
    if store.is_trusted(repository, hash)? {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!(".wtconfig commands are not trusted; review them and run wt trust --yes");
    }
    let commands = format!(
        "bootstrap: {}\nteardown: {}",
        config.bootstrap.as_deref().unwrap_or("(none)"),
        config.teardown.as_deref().unwrap_or("(none)")
    );
    cliclack::note("Commands requested by .wtconfig", commands)?;
    if !cliclack::confirm("Allow and remember these commands?")
        .initial_value(false)
        .interact()?
    {
        bail!("configuration commands were not allowed");
    }
    store.trust(repository, hash)
}

fn run_hook(name: &str, command: &str, directory: &Path, verbose: bool) -> Result<()> {
    let mut hook = Command::new("/bin/sh");
    hook.args(["-c", command]).current_dir(directory);
    let status = if verbose || !ui::terminal() {
        if ui::terminal() {
            eprintln!("  {}", style(command, 2));
        }
        hook.stdout(Stdio::from(std::io::stderr()))
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("start {name} command"))?
    } else {
        // Anonymous file keeps memory bounded and is removed even on failure.
        let mut log = tempfile::tempfile().context("create setup log")?;
        hook.stdout(log.try_clone()?).stderr(log.try_clone()?);
        let (running, complete) = if name == "bootstrap" {
            ("Setting up environment", "Environment ready")
        } else {
            ("Running teardown", "Teardown complete")
        };
        let result = progress(running, complete, || {
            let status = hook
                .status()
                .with_context(|| format!("start {name} command"))?;
            if !status.success() {
                bail!("{name} command failed ({status})");
            }
            Ok(status)
        });
        if result.is_err() {
            eprintln!("\n  {}\n", style(command, 2));
            log.rewind()?;
            io::copy(&mut log, &mut io::stderr())?;
        }
        result?
    };
    if !status.success() {
        bail!("{name} command failed ({status})");
    }
    Ok(())
}

fn run_recorded_teardown(store: &Store, record: &Record, verbose: bool) -> Result<()> {
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
    run_hook("teardown", command, &record.path, verbose)
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

pub(crate) fn repository_slug(repo_root: &Path) -> Result<String> {
    let output = run(Command::new("git").current_dir(repo_root).args([
        "config",
        "--get",
        "remote.origin.url",
    ]))
    .context("read origin remote")?;
    github_slug(String::from_utf8(output.stdout)?.trim())
}

fn github_slug(remote: &str) -> Result<String> {
    let path = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("git@github.com:"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .context("origin must point to a GitHub.com repository")?;
    let path = path.trim_end_matches('/');
    let slug = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = slug.split('/');
    let (Some(owner), Some(repository), None) = (parts.next(), parts.next(), parts.next()) else {
        bail!("origin must contain a GitHub owner and repository");
    };
    if owner.is_empty() || repository.is_empty() {
        bail!("origin must contain a GitHub owner and repository");
    }
    Ok(format!("{owner}/{repository}").to_ascii_lowercase())
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
    let Some(path) = reference.strip_prefix("https://github.com/") else {
        bail!("expected an issue number or a {prefix}<number> URL");
    };
    let Some((repository, number)) = path.rsplit_once("/issues/") else {
        bail!("expected an issue number or a {prefix}<number> URL");
    };
    if !repository.eq_ignore_ascii_case(slug) {
        bail!("expected an issue number or a {prefix}<number> URL");
    }
    number.parse().context("parse issue number from URL")
}

fn add_target(reference: &str, repository: &str) -> Result<AddTarget> {
    if reference.parse::<u64>().is_ok() || reference.starts_with("https://github.com/") {
        return Ok(AddTarget::Issue(issue_number(reference, repository)?));
    }
    let output = Command::new("git")
        .args(["check-ref-format", "--branch", reference])
        .output()?;
    if output.status.success() && String::from_utf8(output.stdout)?.trim() == reference {
        return Ok(AddTarget::Branch(reference.to_owned()));
    }
    bail!("invalid branch name: {reference}")
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

fn git_output_in<const N: usize>(directory: &Path, args: [&str; N]) -> Result<String> {
    let output = run(Command::new("git").current_dir(directory).args(args))?;
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

#[cfg(test)]
mod tests {
    use super::{AddTarget, add_target, github_slug, issue_number};

    #[test]
    fn parses_github_clone_urls() {
        assert_eq!(
            github_slug("https://github.com/acme/example.git").unwrap(),
            "acme/example"
        );
        assert_eq!(
            github_slug("git@github.com:acme/example.git").unwrap(),
            "acme/example"
        );
        assert_eq!(
            github_slug("ssh://git@github.com/acme/example.git").unwrap(),
            "acme/example"
        );
        assert_eq!(
            github_slug("https://github.com/Acme/Example.git").unwrap(),
            "acme/example"
        );
        assert!(github_slug("https://example.com/acme/example.git").is_err());
    }

    #[test]
    fn issue_urls_match_repository_names_case_insensitively() {
        assert_eq!(
            issue_number("https://github.com/Acme/Example/issues/42", "acme/example").unwrap(),
            42
        );
        assert!(issue_number("https://github.com/acme/other/issues/42", "acme/example").is_err());
    }

    #[test]
    fn non_issue_references_are_branch_names() {
        assert!(matches!(
            add_target("feat/local-work", "acme/example").unwrap(),
            AddTarget::Branch(branch) if branch == "feat/local-work"
        ));
        assert!(add_target("bad branch", "acme/example").is_err());
    }
}
