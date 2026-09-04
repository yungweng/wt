use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

#[test]
fn add_creates_issue_worktree_and_prints_only_its_path() {
    let fixture = Fixture::new();
    let output = fixture.wt(["add", "https://github.com/acme/example/issues/42"]);

    assert_success(&output);
    let expected = fixture.worktrees.join("example/fix-42-handle-empty-input");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", expected.display())
    );
    assert!(expected.join(".git").exists());
    assert_eq!(
        git(&expected, ["branch", "--show-current"]),
        "fix/42-handle-empty-input"
    );
}

#[test]
fn add_copies_allowed_env_files_and_rewrites_reserved_ports() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let original_port = occupied.local_addr().unwrap().port();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tenv = .env\n\tcopy = web/.env.local\n\tcompose = true\n\tport = APP_PORT\n",
    );
    fixture.write(
        ".env",
        &format!(
            "APP_PORT={original_port}\nAPP_URL=http://localhost:{original_port}\nSECRET=keep-me\n"
        ),
    );
    fixture.write(
        "web/.env.local",
        &format!("PUBLIC_URL=http://app.localhost:{original_port}\n"),
    );

    let output = fixture.wt(["add", "42"]);
    assert_success(&output);

    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    let root_env = fs::read_to_string(path.join(".env")).unwrap();
    let assigned = env_value(&root_env, "APP_PORT").parse::<u16>().unwrap();
    assert_ne!(assigned, original_port);
    assert!(root_env.contains(&format!("APP_URL=http://localhost:{assigned}")));
    assert!(root_env.contains("SECRET=keep-me"));
    assert!(root_env.contains("COMPOSE_PROJECT_NAME=wt-42-acme-example"));
    let web_env = fs::read_to_string(path.join("web/.env.local")).unwrap();
    assert_eq!(
        web_env,
        format!("PUBLIC_URL=http://app.localhost:{assigned}\n")
    );
}

#[test]
fn list_shows_and_remove_deletes_a_managed_worktree_but_keeps_its_branch() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["add", "42"]));
    let path = fixture.worktrees.join("example/fix-42-handle-empty-input");

    let listed = fixture.wt(["list", "--porcelain"]);
    assert_success(&listed);
    assert_eq!(
        String::from_utf8(listed.stdout).unwrap(),
        format!("42\tfix/42-handle-empty-input\t{}\n", path.display())
    );

    let removed = fixture.wt(["remove", "42"]);
    assert_success(&removed);
    assert!(!path.exists());
    assert_eq!(
        git(
            &fixture.repo,
            ["branch", "--list", "fix/42-handle-empty-input"]
        ),
        "fix/42-handle-empty-input"
    );
}

#[test]
fn remove_refuses_a_changed_copied_file() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tenv = .env\n");
    fixture.write(".env", "SECRET=original\n");
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    fs::write(path.join(".env"), "SECRET=changed\n").unwrap();

    let removed = fixture.wt(["remove", "42"]);

    assert!(!removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stderr).contains("changed managed file: .env"));
    assert!(path.exists());
}

#[test]
fn init_writes_config_and_trusted_bootstrap_runs_in_the_new_worktree() {
    let fixture = Fixture::new();
    let root = fixture.worktrees.to_str().unwrap();
    let initialized = fixture.wt([
        "init",
        "--root",
        root,
        "--base",
        "main",
        "--bootstrap",
        "printf ready > .setup-complete",
        "--yes",
    ]);
    assert_success(&initialized);
    let config = fs::read_to_string(fixture.repo.join(".wtconfig")).unwrap();
    assert!(config.contains("base = main"));
    assert!(config.contains("bootstrap = \"printf ready > .setup-complete\""));
    let global = fs::read_to_string(fixture.config.join("wt/config")).unwrap();
    assert!(global.contains(root));

    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join(".setup-complete")).unwrap(),
        "ready"
    );
}

#[test]
fn port_rewrites_do_not_cascade_between_adjacent_ports() {
    let fixture = Fixture::new();
    let (occupied, first) = adjacent_ports();
    let second = first + 1;
    fixture.write(
        ".wtconfig",
        "[wt]\n\tenv = .env\n\tport = API_PORT\n\tport = WEB_PORT\n",
    );
    fixture.write(
        ".env",
        &format!(
            "API_PORT={first}\nWEB_PORT={second}\nAPI_URL=http://localhost:{first}\nWEB_URL=http://localhost:{second}\n"
        ),
    );

    let added = fixture.wt(["add", "42"]);
    drop(occupied);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    let contents = fs::read_to_string(path.join(".env")).unwrap();
    let api = env_value(&contents, "API_PORT");
    let web = env_value(&contents, "WEB_PORT");
    assert_eq!(contents.matches(&format!("localhost:{api}")).count(), 1);
    assert_eq!(contents.matches(&format!("localhost:{web}")).count(), 1);
    assert_ne!(api, web);
}

#[test]
fn failed_setup_rolls_back_the_worktree_and_can_reuse_the_branch() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tcopy = missing.env\n");
    let path = fixture.worktrees.join("example/fix-42-handle-empty-input");

    let failed = fixture.wt(["add", "42"]);
    assert!(!failed.status.success());
    assert!(!path.exists());
    assert_eq!(
        git(
            &fixture.repo,
            ["branch", "--list", "fix/42-handle-empty-input"]
        ),
        "fix/42-handle-empty-input"
    );

    fixture.write("missing.env", "READY=true\n");
    let retried = fixture.wt(["add", "42"]);
    assert_success(&retried);
    assert!(path.join("missing.env").exists());
}

#[test]
fn changed_config_command_is_blocked_before_worktree_creation() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["init", "--bootstrap", "printf safe > .safe", "--yes"]));
    fixture.write(
        ".wtconfig",
        "[wt]\n\tbase = main\n\tbootstrap = \"printf unsafe > .unsafe\"\n",
    );

    let output = fixture.wt(["add", "42"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not trusted"));
    assert!(
        !fixture
            .worktrees
            .join("example/fix-42-handle-empty-input")
            .exists()
    );
}

#[test]
fn trust_accepts_a_reviewed_config_without_rewriting_it() {
    let fixture = Fixture::new();
    let config = "[wt]\n\tbase = main\n\tbootstrap = \"printf reviewed > .reviewed\"\n";
    fixture.write(".wtconfig", config);

    assert_success(&fixture.wt(["trust", "--yes"]));
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".wtconfig")).unwrap(),
        config
    );
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join(".reviewed")).unwrap(),
        "reviewed"
    );
}

#[test]
fn no_bootstrap_explicitly_skips_an_untrusted_command() {
    let fixture = Fixture::new();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tbase = main\n\tbootstrap = \"printf unsafe > .unsafe\"\n",
    );

    let output = fixture.wt(["add", "42", "--no-bootstrap"]);

    assert_success(&output);
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert!(!path.join(".unsafe").exists());
}

#[test]
fn copy_paths_cannot_escape_the_repository() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tcopy = ../private.env\n");

    let output = fixture.wt(["add", "42"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must stay inside"));
    assert!(
        !fixture
            .worktrees
            .join("example/fix-42-handle-empty-input")
            .exists()
    );
}

#[test]
fn tracked_paths_cannot_be_copied_or_marked_disposable() {
    for setting in ["copy", "disposable"] {
        let fixture = Fixture::new();
        fixture.write(".wtconfig", &format!("[wt]\n\t{setting} = README.md\n"));

        let output = fixture.wt(["add", "42"]);

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("tracked by Git"));
        assert_eq!(
            fs::read_to_string(fixture.repo.join("README.md")).unwrap(),
            "example\n"
        );
    }
}

#[test]
fn parallel_adds_lease_different_ports() {
    let fixture = Fixture::new();
    let port = unused_port();
    fixture.write(".wtconfig", "[wt]\n\tenv = .env\n\tport = APP_PORT\n");
    fixture.write(".env", &format!("APP_PORT={port}\n"));

    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.wt(["add", "41"]));
        let second = scope.spawn(|| fixture.wt(["add", "42"]));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_success(&first);
    assert_success(&second);

    let first_path = PathBuf::from(String::from_utf8(first.stdout).unwrap().trim());
    let second_path = PathBuf::from(String::from_utf8(second.stdout).unwrap().trim());
    let first_env = fs::read_to_string(first_path.join(".env")).unwrap();
    let second_env = fs::read_to_string(second_path.join(".env")).unwrap();
    assert_ne!(
        env_value(&first_env, "APP_PORT"),
        env_value(&second_env, "APP_PORT")
    );
}

#[test]
fn remove_refuses_unknown_files_unless_force_is_explicit() {
    let fixture = Fixture::new();
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    fs::write(path.join("notes.txt"), "keep this\n").unwrap();

    let safe = fixture.wt(["remove", "42"]);
    assert!(!safe.status.success());
    assert!(String::from_utf8_lossy(&safe.stderr).contains("unmanaged file: notes.txt"));
    assert!(path.exists());

    let forced = fixture.wt(["remove", "42", "--force"]);
    assert_success(&forced);
    assert!(!path.exists());
}

#[test]
fn trusted_teardown_runs_before_removal() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt([
        "init",
        "--teardown",
        "printf stopped > ../teardown-ran",
        "--yes",
    ]));
    assert_success(&fixture.wt(["add", "42"]));

    let removed = fixture.wt(["remove", "42"]);

    assert_success(&removed);
    assert_eq!(
        fs::read_to_string(fixture.worktrees.join("example/teardown-ran")).unwrap(),
        "stopped"
    );
}

struct Fixture {
    _temp: TempDir,
    repo: PathBuf,
    worktrees: PathBuf,
    state: PathBuf,
    config: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("example");
        let remote = temp.path().join("remote.git");
        let worktrees = temp.path().join("worktrees");
        let state = temp.path().join("state");
        let config = temp.path().join("config");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&bin).unwrap();
        command(&repo, "git", ["init", "-b", "main"]);
        command(&repo, "git", ["config", "user.email", "dev@example.com"]);
        command(&repo, "git", ["config", "user.name", "Example Dev"]);
        command(&repo, "git", ["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("README.md"), "example\n").unwrap();
        command(&repo, "git", ["add", "README.md"]);
        command(&repo, "git", ["commit", "-m", "initial"]);
        command(
            temp.path(),
            "git",
            ["init", "--bare", remote.to_str().unwrap()],
        );
        command(
            &repo,
            "git",
            ["remote", "add", "origin", remote.to_str().unwrap()],
        );
        command(&repo, "git", ["push", "-u", "origin", "main"]);
        write_fake_gh(&bin);
        Self {
            _temp: temp,
            repo,
            worktrees,
            state,
            config,
            bin,
        }
    }

    fn wt<const N: usize>(&self, args: [&str; N]) -> Output {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        Command::new(env!("CARGO_BIN_EXE_wt"))
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", path)
            .env("WT_WORKTREE_ROOT", &self.worktrees)
            .env("WT_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .output()
            .unwrap()
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.repo.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

fn env_value<'a>(contents: &'a str, key: &str) -> &'a str {
    contents
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap()
}

fn adjacent_ports() -> (TcpListener, u16) {
    for port in 20_000..60_000 {
        if let Ok(first) = TcpListener::bind(("127.0.0.1", port))
            && TcpListener::bind(("127.0.0.1", port + 1)).is_ok()
            && TcpListener::bind(("127.0.0.1", port + 2)).is_ok()
        {
            return (first, port);
        }
    }
    panic!("no three adjacent ports available");
}

fn unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_fake_gh(bin: &Path) {
    let path = bin.join("gh");
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = repo ]; then\n  printf '%s\\n' '{\"nameWithOwner\":\"acme/example\",\"defaultBranchRef\":{\"name\":\"main\"}}'\nelse\n  printf '{\"number\":%s,\"title\":\"Handle empty input\",\"state\":\"OPEN\",\"labels\":[{\"name\":\"bug\"}]}\\n' \"$3\"\nfi\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn command<const N: usize>(dir: &Path, program: &str, args: [&str; N]) {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert_success(&output);
}

fn git<const N: usize>(dir: &Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert_success(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
