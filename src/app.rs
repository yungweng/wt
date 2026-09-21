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
    cleanup,
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
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullHead {
    head_ref_name: String,
    is_cross_repository: bool,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

struct AddPlan {
    issue: Option<u64>,
    branch: String,
    /// Fetch the branch from origin before checkout, as for a pull request head.
    fetch: bool,
    path: PathBuf,
    compose_name: String,
    /// GitHub page of the issue or pull request, shown after setup.
    url: Option<String>,
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
            ui::link(record_url(&record).as_deref());
            println!("{}", record.path.display());
            return Ok(());
        }
    }
    ui::heading(slug.rsplit('/').next().unwrap_or(&slug), &target_key);
    offer_setup(&repo_root)?;
    let plan = target_plan(&repo_root, &slug, target)?;
    // A pull request's head branch often already has its issue's worktree.
    if let Some(record) = store.records()?.into_iter().find(|record| {
        record.repository.eq_ignore_ascii_case(&slug)
            && record.branch == plan.branch
            && record.path.exists()
    }) {
        ui::link(plan.url.as_deref());
        println!("{}", record.path.display());
        return Ok(());
    }
    let config = Config::load(&repo_root)?;
    let config_hash = config.command_fingerprint()?;
    ensure_trusted_commands(&store, &slug, &config, config_hash.as_deref(), no_bootstrap)?;
    let prepared = progress("Creating worktree", "Worktree created", || {
        install_worktree(&store, &config, &repo_root, &slug, &plan, config_hash)
    })?;
    // Ports and the record are saved. Other worktrees can now finish setup.
    drop(lock);
    allow_direnv(&repo_root, &plan);
    run_bootstrap(&config, &plan, no_bootstrap, verbose)?;
    finish_setup(&store, &config, &slug, &plan, &prepared)?;
    ui::ready(
        started,
        no_bootstrap && config.bootstrap.is_some(),
        plan.url.as_deref(),
    );
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
            // GitHub serves pull requests as issues; their URL tells them apart.
            if issue.url.contains("/pull/") {
                let branch = progress("Reading pull request", "Pull request loaded", || {
                    pull_head(repo, number, repository)
                })?;
                return Ok(AddPlan {
                    fetch: true,
                    url: Some(issue.url),
                    ..add_plan(repository, Some(number), branch)?
                });
            }
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
    Ok(AddPlan {
        url: Some(issue.url.clone()),
        ..add_plan(repository, Some(issue.number), branch_name(issue))?
    })
}

/// Built locally: GitHub redirects issue URLs of pull requests to the PR.
fn record_url(record: &Record) -> Option<String> {
    record
        .issue
        .map(|number| format!("https://github.com/{}/issues/{number}", record.repository))
}

fn add_plan(repository: &str, issue: Option<u64>, branch: String) -> Result<AddPlan> {
    let directory = branch.replace('/', "-");
    let repo_name = repository.rsplit('/').next().unwrap_or("repository");
    Ok(AddPlan {
        issue,
        fetch: false,
        url: None,
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
    plan: &AddPlan,
    config_hash: Option<String>,
) -> Result<environment::Prepared> {
    create_worktree(repo_root, config, plan)?;
    let setup = prepare_record(store, config, repo_root, plan, repository, config_hash);
    match setup {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            let cleanup = run_git(
                repo_root,
                ["worktree", "remove", "--force", path_str(&plan.path)?],
            );
            if let Err(cleanup) = cleanup {
                bail!("{error:#}; rollback also failed: {cleanup:#}");
            }
            Err(error)
        }
    }
}

fn prepare_record(
    store: &Store,
    config: &Config,
    repo_root: &Path,
    plan: &AddPlan,
    repository: &str,
    config_hash: Option<String>,
) -> Result<environment::Prepared> {
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
        ports: prepared.ports(),
        copied_files: prepared.copied_files.clone(),
        teardown: config.teardown.clone(),
        disposable: config.disposable.clone(),
        config_hash,
    })?;
    Ok(prepared)
}

/// direnv only loads an `.envrc` it was told to allow. The worktree's copy is
/// identical to the checkout's, so an allowed checkout allows the worktree.
fn allow_direnv(repo_root: &Path, plan: &AddPlan) {
    if !plan.path.join(".envrc").is_file() {
        return;
    }
    match environment::direnv_allowed(repo_root) {
        Ok(Some(true)) => {}
        _ => return,
    }
    let result = progress("Allowing direnv", "direnv allowed", || {
        environment::allow_direnv(repo_root, &plan.path)
    });
    if let Err(error) = result {
        ui::warn(&format!("{error:#}; run direnv allow in the worktree"));
    }
}

/// Configured copy paths that bootstrap generated get their ports rewritten
/// and join the managed files, so removal treats them like copied files.
fn finish_setup(
    store: &Store,
    config: &Config,
    repository: &str,
    plan: &AddPlan,
    prepared: &environment::Prepared,
) -> Result<()> {
    if prepared.pending.is_empty() {
        return Ok(());
    }
    let _lock = store.lock()?;
    let mut record = match plan.issue {
        Some(issue) => store.find_issue(repository, issue)?,
        None => store.find_branch(repository, &plan.branch)?,
    }
    .context("worktree record disappeared during setup")?;
    let missing = environment::complete(
        config,
        &plan.path,
        &plan.compose_name,
        &prepared.assignments,
        &prepared.pending,
        &mut record.copied_files,
    )?;
    store.save(&record)?;
    for path in missing {
        ui::warn(&format!(
            "configured copy path not found in checkout or after bootstrap: {}",
            path.display()
        ));
    }
    Ok(())
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

fn create_worktree(repo: &Path, config: &Config, plan: &AddPlan) -> Result<()> {
    let (path, branch) = (&plan.path, plan.branch.as_str());
    if plan.fetch {
        run_git(repo, ["fetch", "origin", branch])
            .with_context(|| format!("pull request head branch {branch} is not on origin"))?;
    }
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
        // Only new branches need a base, so existing branches skip its lookup.
        let base = base_branch(repo, config, plan.issue)?;
        let start = starting_point(repo, &base)?;
        run_git(
            repo,
            ["worktree", "add", "-b", branch, path_str(path)?, &start],
        )
    }
}

/// Prefers the freshly fetched `origin/<base>` so new worktrees never start
/// from a stale local base branch. Falls back to the local branch only when
/// the base does not exist on `origin`.
fn starting_point(repo: &Path, base: &str) -> Result<String> {
    match run_git(repo, ["fetch", "origin", base]) {
        Ok(()) => Ok(format!("origin/{base}")),
        Err(error) => {
            if local_branch_exists(repo, base)? {
                return Ok(base.to_owned());
            }
            Err(error.context(format!("base branch {base} is not on origin or local")))
        }
    }
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
    if !porcelain && records.is_empty() {
        println!("No managed worktrees.");
    }
    if porcelain {
        for record in records {
            println!(
                "{}\t{}\t{}",
                record
                    .issue
                    .map_or_else(|| "-".to_owned(), |n| n.to_string()),
                record.branch,
                record.path.display()
            );
        }
    } else {
        let mut groups = std::collections::BTreeMap::new();
        for record in records {
            let row = (
                record.issue,
                displayed_branch(&record),
                ui::display_path(&record.path),
            );
            groups
                .entry(record.repository)
                .or_insert_with(Vec::new)
                .push(row);
        }
        for (repository, rows) in groups {
            println!("{}", ui::stdout_style(&repository, 1));
            ui::worktree_table(&repository, &rows, "PATH");
            println!();
        }
    }
    Ok(())
}

fn displayed_branch(record: &Record) -> String {
    let current = if !record.path.exists() {
        "(missing)".to_owned()
    } else {
        // An explicit Git directory prevents discovery of an unrelated parent repo.
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(record.path.join(".git"))
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if branch == "HEAD" {
                    "(detached)".to_owned()
                } else {
                    branch
                }
            }
            _ => "(unavailable)".to_owned(),
        }
    };
    if current == record.branch {
        current
    } else {
        format!("{current} (managed: {})", record.branch)
    }
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
    if !record.path.exists() {
        // Nothing is left to check, tear down, or lose: only the records remain.
        run_git(&repo_root, ["worktree", "prune"])?;
        store.delete(&record)?;
        if std::io::stderr().is_terminal() {
            eprintln!(
                "{} Worktree path was missing; record removed, branch kept",
                style("◇", 32)
            );
        }
        return Ok(());
    }
    if !force {
        progress("Checking worktree", "Safety checks passed", || {
            ensure_safe_to_remove(&record)
        })?;
    }
    if !skip_teardown {
        run_recorded_teardown(&store, &record, verbose)?;
    }
    remove_recorded_worktree(&repo_root, &record, force)?;
    store.delete(&record)?;
    if std::io::stderr().is_terminal() {
        eprintln!("{} Branches kept", style("◇", 32));
    }
    Ok(())
}

#[derive(Default)]
struct MergeChecks {
    pulls: std::sync::OnceLock<std::result::Result<Vec<PullRequest>, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    number: u64,
    state: String,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    is_cross_repository: bool,
}

/// What `wt clean` found for a worktree that may be removed.
enum Inspection {
    /// Merged into the base. Non-empty `changes` would be lost and need `--force`.
    Merged {
        head: String,
        reason: String,
        changes: Vec<String>,
    },
    /// The worktree directory no longer exists; only its records remain.
    Missing,
}

struct Candidate {
    record: Record,
    inspection: Inspection,
}

impl Candidate {
    /// A missing directory has nothing left to lose, so only local changes
    /// need `--force`.
    fn needs_force(&self) -> bool {
        match &self.inspection {
            Inspection::Merged { changes, .. } => !changes.is_empty(),
            Inspection::Missing => false,
        }
    }

    fn reason(&self) -> String {
        match &self.inspection {
            Inspection::Merged {
                reason, changes, ..
            } => match changes.as_slice() {
                [] => reason.clone(),
                [change] => format!("{reason}; {}", short_reason(change)),
                [change, rest @ ..] => {
                    format!("{reason}; {} (+{} more)", short_reason(change), rest.len())
                }
            },
            Inspection::Missing => "worktree path is missing".to_owned(),
        }
    }
}

fn short_reason(reason: &str) -> String {
    reason
        .replace("worktree contains an unmanaged file: ", "Unmanaged: ")
        .replace("worktree contains tracked changes: ", "Modified: ")
        .replace(
            "no merged PR found for this branch into ",
            "No merged PR into ",
        )
}

pub fn clean(
    dry_run: bool,
    yes: bool,
    force: bool,
    skip_teardown: bool,
    verbose: bool,
) -> Result<()> {
    let repo = PathBuf::from(git_output(["rev-parse", "--show-toplevel"])?);
    let repository = repository_slug(&repo)?;
    let store = Store::open_readonly()?;
    let mut records = store.records()?;
    records.retain(|record| record.repository.eq_ignore_ascii_case(&repository));
    records.sort_by(|a, b| a.branch.cmp(&b.branch));
    if records.is_empty() {
        println!("No managed worktrees in this repository.");
        return Ok(());
    }
    let config = Config::load(&repo)?;
    let base = match config.base {
        Some(base) => base,
        None => self::repository(&repo)?.default_branch.name,
    };
    // Resolve once for the preview; never assume the calling branch is the base.
    let base_commit = git_output_in(
        &repo,
        ["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .with_context(|| format!("cannot resolve base {base}; update it before running wt clean"))?;
    let checks = MergeChecks::default();
    // Read-only inspection is bounded to four workers.
    let inspections = std::thread::scope(|scope| -> Result<Vec<_>> {
        let workers = records
            .chunks(records.len().div_ceil(4))
            .map(|chunk| {
                let (repo, base, base_commit, checks) = (&repo, &base, &base_commit, &checks);
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|record| clean_candidate(repo, record, base, base_commit, checks))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let mut results = Vec::new();
        for worker in workers {
            results.extend(
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("worktree inspection panicked"))?,
            );
        }
        Ok(results)
    })?;
    let mut candidates = Vec::new();
    let mut forced = Vec::new();
    let mut skipped = Vec::new();
    for (record, inspection) in records.into_iter().zip(inspections) {
        match inspection {
            Ok(inspection) => {
                let candidate = Candidate { record, inspection };
                if candidate.needs_force() && !force {
                    forced.push(candidate);
                } else {
                    candidates.push(candidate);
                }
            }
            Err(error) => skipped.push((record, format!("{error:#}"))),
        }
    }
    let rows = |candidates: &[Candidate]| {
        candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.record.issue,
                    candidate.record.branch.clone(),
                    candidate.reason(),
                )
            })
            .collect::<Vec<_>>()
    };
    println!("{}", ui::stdout_style(&repository, 1));
    if !candidates.is_empty() {
        println!(
            "\n{}",
            ui::stdout_style(&format!("Ready to remove ({})", candidates.len()), 32)
        );
        ui::worktree_table(&repository, &rows(&candidates), "REASON");
    }
    if !forced.is_empty() {
        println!(
            "\n{}",
            ui::stdout_style(&format!("Needs --force ({})", forced.len()), 33)
        );
        ui::worktree_table(&repository, &rows(&forced), "REASON");
    }
    if !skipped.is_empty() {
        println!(
            "\n{}",
            ui::stdout_style(&format!("Skipped ({})", skipped.len()), 33)
        );
        let rows = skipped
            .iter()
            .map(|(record, reason)| (record.issue, record.branch.clone(), short_reason(reason)))
            .collect::<Vec<_>>();
        ui::worktree_table(&repository, &rows, "REASON");
    }
    println!();
    if !forced.is_empty() {
        println!(
            "{} merged worktree(s) kept for local changes. Use wt clean --force to remove them too.",
            forced.len()
        );
    }
    if candidates.is_empty() {
        println!("No safely merged worktrees to remove.");
        return Ok(());
    }
    let losing = candidates.iter().filter(|c| c.needs_force()).count();
    if losing > 0 {
        println!("{losing} candidate(s) lose the local changes shown above.");
    }
    println!("{} candidate(s). Branches will be kept.", candidates.len());
    if dry_run {
        return Ok(());
    }
    if !yes {
        if !io::stdin().is_terminal() {
            bail!("cleanup needs confirmation; use --dry-run to preview or --yes to remove");
        }
        if !cliclack::confirm("Remove these worktrees?")
            .initial_value(false)
            .interact()?
        {
            println!("Cancelled. No worktrees removed.");
            return Ok(());
        }
    }
    let store = Store::open()?;
    let started = Instant::now();
    let total = candidates.len();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (sender, receiver) = std::sync::mpsc::channel();
    // Verbose output streams raw logs, so it stays serial and readable.
    let workers = if verbose { 1 } else { total.min(4) };
    let (mut removed, mut failed) = (0, 0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let (store, repo, base, repository, checks, candidates, next) = (
                &store,
                &repo,
                &base,
                &repository,
                &checks,
                &candidates,
                &next,
            );
            scope.spawn(move || {
                ui::set_quiet(!verbose);
                while let Some(candidate) =
                    candidates.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                {
                    let result = remove_candidate(
                        store,
                        repo,
                        repository,
                        base,
                        checks,
                        candidate,
                        skip_teardown,
                    );
                    if sender.send((&candidate.record.branch, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        let mut done = 0;
        loop {
            ui::status(&format!("Removing worktrees {done}/{total}"), started);
            match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok((branch, result)) => {
                    done += 1;
                    ui::clear_status();
                    match result {
                        Ok(()) => {
                            removed += 1;
                            println!("Removed {branch}");
                        }
                        Err(error) => {
                            failed += 1;
                            eprintln!("Skipped {branch}: {error:#}");
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        ui::clear_status();
    });
    // Pruning while other removals run could drop an entry Git is still removing.
    // Prune only drops Git's records of worktrees whose directory is gone.
    if candidates
        .iter()
        .any(|candidate| matches!(candidate.inspection, Inspection::Missing))
    {
        if let Err(error) = run_git(&repo, ["worktree", "prune"]) {
            failed += 1;
            eprintln!("Could not prune missing worktrees: {error:#}");
        }
    }
    println!(
        "Removed {removed} worktree(s) in {:.1}s. Branches kept.",
        started.elapsed().as_secs_f64()
    );
    if failed > 0 {
        bail!("{failed} candidate(s) could not be removed");
    }
    Ok(())
}

/// Rechecks a previewed candidate and removes it. Changes that were not in
/// the preview are never discarded, even with `--force`.
fn remove_candidate(
    store: &Store,
    repo: &Path,
    repository: &str,
    base: &str,
    checks: &MergeChecks,
    candidate: &Candidate,
    skip_teardown: bool,
) -> Result<()> {
    let preview = &candidate.record;
    let reference = preview
        .issue
        .map_or_else(|| preview.branch.clone(), |n| n.to_string());
    let _worktree_lock = store.lock_worktree(repository, &reference)?;
    let record = {
        let _lock = store.lock()?;
        match preview.issue {
            Some(issue) => store.find_issue(repository, issue)?,
            None => store.find_branch(repository, &preview.branch)?,
        }
        .context("worktree is no longer managed")?
    };
    if serde_json::to_value(&record)? != serde_json::to_value(preview)? {
        bail!("worktree record changed after preview; run wt clean again");
    }
    let (head, previewed) = match &candidate.inspection {
        Inspection::Missing => {
            match fs::symlink_metadata(&record.path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => bail!("worktree path reappeared after preview; run wt clean again"),
            }
            // Git's stale entry is pruned once all workers have finished.
            let _lock = store.lock()?;
            return store.delete(&record);
        }
        Inspection::Merged { head, changes, .. } => (head, changes),
    };
    let current_base = git_output_in(
        repo,
        ["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )?;
    let recheck = |stage: &str| -> Result<()> {
        let Inspection::Merged {
            head: current,
            changes,
            ..
        } = clean_candidate(repo, &record, base, &current_base, checks)?
        else {
            bail!("worktree path disappeared {stage}");
        };
        if &current != head {
            bail!("HEAD changed {stage}");
        }
        if let Some(change) = changes.iter().find(|change| !previewed.contains(change)) {
            bail!("{change}");
        }
        Ok(())
    };
    recheck("after preview; run wt clean again")?;
    if !skip_teardown {
        run_recorded_teardown(store, &record, false)?;
    }
    // A teardown may itself change files or switch branches.
    recheck("during teardown")?;
    remove_recorded_worktree(repo, &record, !previewed.is_empty())?;
    let _lock = store.lock()?;
    store.delete(&record)
}

fn clean_candidate(
    repo: &Path,
    record: &Record,
    base: &str,
    base_commit: &str,
    checks: &MergeChecks,
) -> Result<Inspection> {
    match fs::symlink_metadata(&record.path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Inspection::Missing),
        _ => {}
    }
    let path = fs::canonicalize(&record.path).context("worktree path is unavailable")?;
    if fs::canonicalize(std::env::current_dir()?)?.starts_with(&path) {
        bail!("current worktree");
    }
    // Only linked worktrees in this checkout are eligible, not another clone
    // of the same GitHub repository or a stale path pointing at its parent.
    if !path.join(".git").is_file() {
        bail!("not a linked worktree");
    }
    let common = |directory: &Path| -> Result<PathBuf> {
        let value = git_output_in(directory, ["rev-parse", "--git-common-dir"])?;
        Ok(fs::canonicalize(directory.join(value))?)
    };
    if common(&path)? != common(repo)? {
        bail!("worktree belongs to another clone");
    }
    let git_dir = PathBuf::from(git_output_in(&path, ["rev-parse", "--absolute-git-dir"])?);
    if git_dir.join("locked").try_exists()? {
        bail!("worktree is locked");
    }
    let branch = git_output_in(&path, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("detached or unavailable HEAD")?;
    if branch != record.branch {
        bail!("checked-out branch differs from managed branch");
    }
    if branch
        == base
            .trim_start_matches("refs/heads/")
            .trim_start_matches("refs/remotes/")
            .trim_start_matches("origin/")
    {
        bail!("base branch");
    }
    let head = git_output_in(&path, ["rev-parse", "--verify", "HEAD"])?;
    let output = Command::new("git")
        .current_dir(repo)
        .args(["merge-base", "--is-ancestor", &head, base_commit])
        .output()?;
    let (mut reason, proven_by_pr) = match output.status.code() {
        Some(0) => (format!("merged into {base}"), false),
        Some(1) => {
            let number = merged_pull_request(repo, record, base, &head, checks)?
                .with_context(|| format!("no merged PR found for this branch into {base}"))?;
            (format!("merged PR #{number} into {base}"), true)
        }
        _ => bail!(
            "cannot check merge status: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    };
    // Checked after the merge status so unmerged work is never offered for removal.
    let changes = blocking_changes(record)?;
    // A new branch without commits is also an ancestor of the base. Only a
    // merged PR shows that its local changes belong to finished work.
    if !changes.is_empty() && !proven_by_pr {
        match merged_pull_request(repo, record, base, &head, checks) {
            Ok(Some(number)) => reason = format!("merged PR #{number} into {base}"),
            // The local change explains more than a failed or empty PR lookup.
            _ => bail!("{}", changes[0]),
        }
    }
    Ok(Inspection::Merged {
        head,
        reason,
        changes,
    })
}

fn merged_pull_request(
    repo: &Path,
    record: &Record,
    base: &str,
    head: &str,
    checks: &MergeChecks,
) -> Result<Option<u64>> {
    let base = base
        .trim_start_matches("refs/heads/")
        .trim_start_matches("refs/remotes/")
        .trim_start_matches("origin/");
    let prs = checks
        .pulls
        .get_or_init(|| {
            query_merged_pull_requests(repo, &record.repository, base, None)
                .map_err(|error| format!("{error:#}"))
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!(error.clone()))?;
    let mut matched = None;
    let mut check = |prs: &[PullRequest]| -> Result<Option<u64>> {
        for pr in prs.iter().filter(|pr| {
            pr.state == "MERGED"
                && !pr.is_cross_repository
                && pr.head_ref_name == record.branch
                && pr.base_ref_name == base
        }) {
            matched = Some(pr.number);
            if included_in_merged_head(repo, &record.repository, head, &pr.head_ref_oid)? {
                return Ok(Some(pr.number));
            }
        }
        Ok(None)
    };
    if let Some(number) = check(prs)? {
        return Ok(Some(number));
    }
    // A full recent page is not proof that an older branch has no merged PR.
    if prs.len() == 100 {
        let older =
            query_merged_pull_requests(repo, &record.repository, base, Some(&record.branch))?;
        if let Some(number) = check(&older)? {
            return Ok(Some(number));
        }
    }
    if let Some(number) = matched {
        bail!("PR #{number} is merged; local commits are not included in its merged head");
    }
    Ok(None)
}

fn query_merged_pull_requests(
    repo: &Path,
    repository: &str,
    base: &str,
    branch: Option<&str>,
) -> Result<Vec<PullRequest>> {
    let mut command = Command::new("gh");
    command.current_dir(repo).args([
        "pr",
        "list",
        "--repo",
        repository,
        "--state",
        "merged",
        "--base",
        base,
        "--limit",
        "100",
        "--json",
        "number,state,headRefName,headRefOid,baseRefName,isCrossRepository",
    ]);
    if let Some(branch) = branch {
        command.args(["--head", branch]);
    }
    let output = run(&mut command).context("cannot verify merged PRs; keeping worktree")?;
    serde_json::from_slice(&output.stdout).context("cannot parse merged PRs; keeping worktree")
}

fn included_in_merged_head(
    repo: &Path,
    repository: &str,
    head: &str,
    merged: &str,
) -> Result<bool> {
    if head == merged {
        return Ok(true);
    }
    // Resolve locally when possible; comparison on GitHub avoids fetching or
    // changing refs when the PR's final commit only exists on the server.
    let known = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-e", &format!("{merged}^{{commit}}")])
        .output()?;
    if known.status.success() {
        let output = Command::new("git")
            .current_dir(repo)
            .args(["merge-base", "--is-ancestor", head, merged])
            .output()?;
        return match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("cannot compare local commits with merged PR"),
        };
    }
    let output = run(Command::new("gh").current_dir(repo).args([
        "api",
        &format!("repos/{repository}/compare/{head}...{merged}"),
        "--jq",
        ".status",
    ]))
    .context("PR is merged, but cannot verify whether it includes local commits")?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "ahead" | "identical" => Ok(true),
        "behind" | "diverged" => Ok(false),
        _ => bail!("PR is merged, but GitHub returned an unknown commit comparison"),
    }
}

fn remove_recorded_worktree(repo: &Path, record: &Record, force: bool) -> Result<()> {
    let trash = cleanup::trash_dir(&record.path).context("worktree path has no parent")?;
    let cleared = progress(
        "Clearing generated files",
        "Generated files cleared",
        || delete_managed_files(record, &trash, force),
    );
    // Forced removal lets Git delete whatever could not be moved aside.
    if !force {
        cleared?;
    }
    let removed = progress("Removing worktree", "Worktree removed", || {
        let path = path_str(&record.path)?;
        if force {
            run_git(repo, ["worktree", "remove", "--force", path])
        } else {
            run_git(repo, ["worktree", "remove", path])
        }
    });
    cleanup::purge_in_background(&trash);
    removed
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
    // Parallel cleanup captures logs even without a terminal, so output stays readable.
    let status = if verbose || (!ui::terminal() && !ui::quiet()) {
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
            log.rewind()?;
            let mut bytes = Vec::new();
            io::Read::read_to_end(&mut log, &mut bytes)?;
            let output = String::from_utf8_lossy(&bytes);
            if ui::quiet() {
                return result
                    .map(|_| ())
                    .with_context(|| format!("{command}\n{}", output.trim_end()));
            }
            eprintln!("\n  {}\n", style(command, 2));
            eprint!("{output}");
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
    match blocking_changes(record)?.into_iter().next() {
        Some(change) => bail!("{change}"),
        None => Ok(()),
    }
}

/// Every local change that removal without `--force` would lose.
fn blocking_changes(record: &Record) -> Result<Vec<String>> {
    let mut changes = Vec::new();
    for (path, expected) in &record.copied_files {
        let actual = environment::fingerprint(&record.path, path)?;
        if &actual != expected {
            changes.push(format!("changed managed file: {}", path.display()));
        }
    }
    for (kind, path) in worktree_changes(&record.path)? {
        if kind != "??" && kind != "!!" {
            changes.push(format!("worktree contains tracked changes: {path}"));
        } else if !is_removable_path(Path::new(&path), record) {
            if path.ends_with('/') {
                let directory = Path::new(path.trim_end_matches('/'));
                if kind == "!!" && contains_only_directories(&record.path.join(directory))? {
                    continue;
                }
                // A copied directory shows up collapsed; its files are managed.
                if contains_only_managed_files(&record.path, directory, record)? {
                    continue;
                }
            }
            changes.push(format!("worktree contains an unmanaged file: {path}"));
        }
    }
    Ok(changes)
}

/// Whether every file below `directory` (relative to the worktree) is a
/// managed copied file, without following symlinked directories.
fn contains_only_managed_files(root: &Path, directory: &Path, record: &Record) -> Result<bool> {
    let path = root.join(directory);
    let metadata =
        fs::symlink_metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_dir() {
        return Ok(false);
    }
    let mut managed = false;
    for entry in fs::read_dir(&path).with_context(|| format!("read {}", path.display()))? {
        let entry = entry.with_context(|| format!("read {}", path.display()))?;
        let relative = directory.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if !contains_only_managed_files(root, &relative, record)? {
                return Ok(false);
            }
        } else if record.copied_files.contains_key(&relative) {
            managed = true;
        } else {
            return Ok(false);
        }
    }
    Ok(managed)
}

fn contains_only_directories(path: &Path) -> Result<bool> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let entry = entry.with_context(|| format!("read {}", path.display()))?;
        if !contains_only_directories(&entry.path())? {
            return Ok(false);
        }
    }
    Ok(true)
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

fn delete_managed_files(record: &Record, trash: &Path, force: bool) -> Result<()> {
    for path in record.copied_files.keys() {
        match fs::remove_file(record.path.join(path)) {
            Err(error) if !(force && error.kind() == io::ErrorKind::NotFound) => {
                return Err(error)
                    .with_context(|| format!("remove {}", record.path.join(path).display()));
            }
            _ => {}
        }
    }
    cleanup::trash_paths(&record.path, &record.disposable, trash)
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
        "number,title,state,labels,url",
    ]))?;
    serde_json::from_slice(&output.stdout).context("parse issue details from gh")
}

/// Returns the head branch of a same-repository pull request.
fn pull_head(repo_root: &Path, number: u64, slug: &str) -> Result<String> {
    let output = run(Command::new("gh").current_dir(repo_root).args([
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        slug,
        "--json",
        "headRefName,isCrossRepository",
    ]))?;
    let head: PullHead =
        serde_json::from_slice(&output.stdout).context("parse pull request details from gh")?;
    if head.is_cross_repository {
        bail!(
            "pull request #{number} comes from a fork; check it out with gh pr checkout {number}"
        );
    }
    Ok(head.head_ref_name)
}

fn issue_number(reference: &str, slug: &str) -> Result<u64> {
    if let Ok(number) = reference.parse() {
        return Ok(number);
    }
    let expected = format!(
        "expected a number or a https://github.com/{slug}/issues/<number> or /pull/<number> URL"
    );
    let Some(path) = reference.strip_prefix("https://github.com/") else {
        bail!("{expected}");
    };
    let Some((repository, number)) = path
        .rsplit_once("/issues/")
        .or_else(|| path.rsplit_once("/pull/"))
    else {
        bail!("{expected}");
    };
    if !repository.eq_ignore_ascii_case(slug) {
        bail!("{expected}");
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
        let transliteration = match character {
            'ä' => "ae",
            'ö' => "oe",
            'ü' => "ue",
            'ß' => "ss",
            _ => "",
        };
        if !transliteration.is_empty() {
            slug.push_str(transliteration);
        } else if character.is_ascii_alphanumeric() {
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
    use super::{AddTarget, add_target, github_slug, issue_number, slugify};

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
        assert_eq!(
            issue_number("https://github.com/Acme/Example/pull/7", "acme/example").unwrap(),
            7
        );
        assert!(issue_number("https://github.com/acme/other/pull/7", "acme/example").is_err());
    }

    #[test]
    fn non_issue_references_are_branch_names() {
        assert!(matches!(
            add_target("feat/local-work", "acme/example").unwrap(),
            AddTarget::Branch(branch) if branch == "feat/local-work"
        ));
        assert!(add_target("bad branch", "acme/example").is_err());
    }

    #[test]
    fn slugs_are_ascii_and_transliterate_german_letters() {
        let slug = slugify("Kiosk: Schulhof-Wahl bei laufenden Blöcken bucht sonst Freispiel");
        assert!(slug.starts_with("kiosk-schulhof-wahl-bei-laufenden-bloecken-bucht"));
        assert!(slug.is_ascii());
        assert_eq!(slugify("Größe ÄÖÜ"), "groesse-aeoeue");
        assert_eq!(slugify("Café naïve"), "caf-na-ve");
        assert_eq!(slugify("日本"), "issue");
    }
}
