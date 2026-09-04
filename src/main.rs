mod app;
mod config;
mod environment;
mod state;

use std::io::IsTerminal;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "wt",
    version,
    about = "Create isolated Git worktrees from GitHub issues"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Configure wt for the current repository
    Init(InitArgs),
    /// Trust commands in the current .wtconfig
    Trust {
        /// Trust the reviewed file without prompting
        #[arg(long)]
        yes: bool,
    },
    /// Create a worktree for a GitHub issue
    Add {
        /// Issue number or GitHub issue URL
        issue: String,
        /// Create the worktree without running its bootstrap command
        #[arg(long)]
        no_bootstrap: bool,
    },
    /// List worktrees created by wt
    List {
        /// Print stable tab-separated output
        #[arg(long)]
        porcelain: bool,
        /// Include worktrees from every repository
        #[arg(long)]
        all: bool,
    },
    /// Safely remove a worktree without deleting its branch
    Remove {
        /// Issue number
        issue: u64,
        /// Remove even when the worktree contains changes
        #[arg(long)]
        force: bool,
        /// Do not run the configured teardown command
        #[arg(long)]
        skip_teardown: bool,
    },
}

#[derive(Args)]
struct InitArgs {
    /// Global directory that contains worktrees
    #[arg(long)]
    root: Option<String>,
    /// Branch used as the starting point
    #[arg(long)]
    base: Option<String>,
    /// Primary env file copied and updated in each worktree
    #[arg(long)]
    env: Option<String>,
    /// Additional file to copy; may be repeated
    #[arg(long)]
    copy: Vec<String>,
    /// Add a unique COMPOSE_PROJECT_NAME to the primary env file
    #[arg(long)]
    compose: bool,
    /// Port variable from the primary env file; may be repeated
    #[arg(long)]
    port: Vec<String>,
    /// Trusted shell command run after setup
    #[arg(long)]
    bootstrap: Option<String>,
    /// Trusted shell command run before removal
    #[arg(long)]
    teardown: Option<String>,
    /// Generated path that may be discarded; may be repeated
    #[arg(long)]
    disposable: Vec<String>,
    /// Write and trust the supplied configuration without prompts
    #[arg(long)]
    yes: bool,
}

fn main() {
    if let Err(error) = run() {
        if std::io::stderr().is_terminal() {
            let _ = cliclack::log::error(format!("{error:#}"));
        } else {
            eprintln!("error: {error:#}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Init(args) => app::init(app::InitOptions {
            root: args.root,
            base: args.base,
            env: args.env,
            copies: args.copy,
            compose: args.compose,
            ports: args.port,
            bootstrap: args.bootstrap,
            teardown: args.teardown,
            disposable: args.disposable,
            yes: args.yes,
        }),
        Command::Trust { yes } => app::trust(yes),
        Command::Add {
            issue,
            no_bootstrap,
        } => app::add(&issue, no_bootstrap),
        Command::List { porcelain, all } => app::list(porcelain, all),
        Command::Remove {
            issue,
            force,
            skip_teardown,
        } => app::remove(issue, force, skip_teardown),
    }
}
