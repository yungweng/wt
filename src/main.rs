mod app;
mod cleanup;
mod completion;
mod config;
mod detection;
mod environment;
mod state;
mod ui;

use anyhow::Result;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueHint};
use clap_complete::engine::ArgValueCompleter;

#[derive(Parser)]
#[command(
    name = "wt",
    version,
    about = "Create isolated, ready-to-code Git worktrees"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Stream raw setup and teardown output
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Configure wt for the current repository
    Init(InitArgs),
    /// Trust commands in the current .wtconfig
    #[command(hide = true)]
    Trust {
        /// Trust the reviewed file without prompting
        #[arg(long)]
        yes: bool,
    },
    /// Create a worktree for a GitHub issue or branch
    Add {
        /// Issue number, GitHub issue URL, or branch name
        #[arg(add = ArgValueCompleter::new(completion::branches))]
        reference: String,
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
    /// Preview and remove safely merged worktrees in the current repository
    Clean {
        /// Show candidates without removing anything or running teardown
        #[arg(long, conflicts_with = "yes")]
        dry_run: bool,
        /// Remove the previewed candidates without prompting
        #[arg(long)]
        yes: bool,
        /// Do not run configured teardown commands
        #[arg(long)]
        skip_teardown: bool,
    },
    /// Safely remove a worktree without deleting its branch
    Remove {
        /// Issue number or branch name
        #[arg(add = ArgValueCompleter::new(completion::worktrees))]
        reference: String,
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
    #[arg(long, value_hint = ValueHint::DirPath)]
    root: Option<String>,
    /// Branch used as the starting point
    #[arg(long, add = ArgValueCompleter::new(completion::branches))]
    base: Option<String>,
    /// Primary env file copied and updated in each worktree
    #[arg(long, value_hint = ValueHint::FilePath)]
    env: Option<String>,
    /// Additional file to copy; may be repeated
    #[arg(long, value_hint = ValueHint::FilePath)]
    copy: Vec<String>,
    /// Add a unique COMPOSE_PROJECT_NAME to the primary env file
    #[arg(long)]
    compose: bool,
    /// Env port KEY or process port KEY:DEFAULT; may be repeated
    #[arg(long)]
    port: Vec<String>,
    /// Trusted shell command run after setup
    #[arg(long)]
    bootstrap: Option<String>,
    /// Trusted shell command run before removal
    #[arg(long)]
    teardown: Option<String>,
    /// Generated path that may be discarded; may be repeated
    #[arg(long, value_hint = ValueHint::AnyPath)]
    disposable: Vec<String>,
    /// Write and trust the supplied configuration without prompts
    #[arg(long)]
    yes: bool,
}

fn main() {
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .shells(clap_complete::env::Shells(&[
            &clap_complete::env::Bash,
            &clap_complete::env::Zsh,
            &completion::Fish,
        ]))
        .complete();
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    ui::init();
    let cli = Cli::parse();
    match cli.command {
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
            reference,
            no_bootstrap,
        } => app::add(&reference, no_bootstrap, cli.verbose),
        Command::List { porcelain, all } => app::list(porcelain, all),
        Command::Clean {
            dry_run,
            yes,
            skip_teardown,
        } => app::clean(dry_run, yes, skip_teardown, cli.verbose),
        Command::Remove {
            reference,
            force,
            skip_teardown,
        } => app::remove(&reference, force, skip_teardown, cli.verbose),
    }
}
