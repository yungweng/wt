use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::Duration,
};

use tempfile::TempDir;

#[test]
fn completion_registers_shells_and_suggests_commands_flags_and_paths() {
    let fixture = Fixture::new();
    let outside = fixture._temp.path();
    for shell in ["bash", "zsh", "fish"] {
        let output = fixture
            .wt_command([])
            .current_dir(outside)
            .env("COMPLETE", shell)
            .output()
            .unwrap();
        assert_success(&output);
        assert!(output.stderr.is_empty());
        assert!(String::from_utf8_lossy(&output.stdout).contains("wt"));
    }
    assert_eq!(fixture.complete(&["a"]), ["add"]);
    assert!(!fixture.complete(&[""]).contains(&"trust".to_owned()));
    assert_eq!(fixture.complete(&["add", "--no"]), ["--no-bootstrap"]);
    assert_eq!(
        fixture.complete(&["-v", "remove", "--skip"]),
        ["--skip-teardown"]
    );

    fixture.write("env files/dev.env", "PORT=3000\n");
    for option in ["--env", "--copy"] {
        assert_eq!(
            fixture.complete(&["init", option, "env files/d"]),
            ["env files/dev.env"]
        );
    }
    assert_eq!(fixture.complete(&["init", "--root", "env"]), ["env files/"]);
    assert!(
        fixture
            .complete(&["init", "--root", "env files/d"])
            .is_empty()
    );
    assert_eq!(
        fixture.complete(&["init", "--disposable", "env files/d"]),
        ["env files/dev.env"]
    );
    assert!(!fixture.state.exists());
}

#[test]
fn fish_completion_handles_quoted_escaped_and_empty_tokens() {
    if Command::new("fish").arg("--version").output().is_err() {
        eprintln!("fish is not installed; skipping shell integration test");
        return;
    }
    let fixture = Fixture::new();
    fixture.write("env files/dev.env", "PORT=3000\n");
    let output = Command::new("fish")
        .args([
            "--no-config",
            "-c",
            r#"
COMPLETE=fish "$WT_BINARY" | source
complete -C "wt init --env 'env files/d"
complete -C 'wt init --env env\ files/d'
complete -C 'wt init --env "env files/d'
complete -C 'wt '
"#,
        ])
        .env("WT_BINARY", env!("CARGO_BIN_EXE_wt"))
        .env("WT_STATE_HOME", &fixture.state)
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert_success(&output);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    let completions = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        completions.matches("env files/dev.env").count(),
        3,
        "{completions}"
    );
    assert!(completions.lines().any(|line| line.starts_with("add\t")));
    assert!(!fixture.state.exists());
}

#[test]
fn completion_filters_and_deduplicates_local_and_origin_branches() {
    let fixture = Fixture::new();
    command(&fixture.repo, "git", ["branch", "feat/local"]);
    command(
        &fixture.repo,
        "git",
        ["update-ref", "refs/remotes/origin/feat/local", "HEAD"],
    );
    command(
        &fixture.repo,
        "git",
        ["update-ref", "refs/remotes/origin/feat/remote", "HEAD"],
    );
    command(
        &fixture.repo,
        "git",
        ["update-ref", "refs/remotes/other/feat/ignored", "HEAD"],
    );
    command(
        &fixture.repo,
        "git",
        [
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    fs::write(
        fixture.bin.join("gh"),
        "#!/bin/sh\ntouch \"${0}-called\"\nexit 99\n",
    )
    .unwrap();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tbootstrap = touch hook-called\n\tteardown = touch hook-called\n",
    );

    for args in [vec!["add", "feat/"], vec!["init", "--base", "feat/"]] {
        assert_eq!(fixture.complete(&args), ["feat/local", "feat/remote"]);
    }
    assert_eq!(fixture.complete(&["add", "feat/r"]), ["feat/remote"]);
    assert!(!fixture.complete(&["add", ""]).contains(&"HEAD".to_owned()));
    assert!(!fixture.state.exists());
    assert!(!fixture.bin.join("gh-called").exists());
    assert!(!fixture.repo.join("hook-called").exists());
    assert!(!fixture.repo.join(".git/FETCH_HEAD").exists());
}

#[test]
fn completion_suggests_only_valid_removal_targets_for_current_repository() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["add", "42"]));
    assert_success(&fixture.wt(["add", "feat/local"]));
    let record_path = fixture.state.join("records/acme--example--42.json");
    let original = fs::read_to_string(&record_path).unwrap();
    fs::write(
        &record_path,
        original.replace("acme/example", "Acme/Example"),
    )
    .unwrap();
    fs::write(
        fixture.state.join("records/other--repo--42.json"),
        original
            .replace("acme/example", "other/repo")
            .replace("\"issue\": 42", "\"issue\": 99"),
    )
    .unwrap();
    let before = fs::read(&record_path).unwrap();
    let lock_modified = fs::metadata(fixture.state.join("lock"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(
        fixture.bin.join("gh"),
        "#!/bin/sh\ntouch \"${0}-called\"\nexit 99\n",
    )
    .unwrap();

    let targets = fixture.complete(&["remove", ""]);
    assert!(targets.contains(&"42".to_owned()));
    assert!(targets.contains(&"feat/local".to_owned()));
    assert!(!targets.contains(&"99".to_owned()));
    assert!(!targets.contains(&"fix/42-handle-empty-input".to_owned()));
    assert_eq!(fixture.complete(&["remove", "feat/"]), ["feat/local"]);
    assert_eq!(fixture.complete(&["remove", "4"]), ["42"]);
    assert_eq!(fs::read(record_path).unwrap(), before);
    assert_eq!(
        fs::metadata(fixture.state.join("lock"))
            .unwrap()
            .modified()
            .unwrap(),
        lock_modified
    );
    assert!(!fixture.bin.join("gh-called").exists());
}

#[test]
fn completion_is_silent_without_repository_or_readable_state() {
    let fixture = Fixture::new();
    assert!(fixture.complete(&["remove", "missing"]).is_empty());
    assert!(!fixture.state.exists());
    fs::create_dir_all(fixture.state.join("records")).unwrap();
    let corrupt = fixture.state.join("records/corrupt.json");
    fs::write(&corrupt, "not json").unwrap();
    assert!(fixture.complete(&["remove", "missing"]).is_empty());
    assert_eq!(fs::read_to_string(corrupt).unwrap(), "not json");
    assert!(!fixture.state.join("lock").exists());

    for subcommand in ["add", "remove"] {
        let output = fixture
            .completion_command(&[subcommand, "missing"])
            .current_dir(fixture._temp.path())
            .output()
            .unwrap();
        assert_success(&output);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

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
fn add_manages_a_branch_worktree_without_calling_github() {
    let fixture = Fixture::new();
    fixture.fail_gh_calls();

    let added = fixture.wt(["add", "feat/local-work"]);

    assert_success(&added);
    let expected = fixture.worktrees.join("example/feat-local-work");
    assert_eq!(
        String::from_utf8(added.stdout).unwrap(),
        format!("{}\n", expected.display())
    );
    assert_eq!(
        git(&expected, ["branch", "--show-current"]),
        "feat/local-work"
    );
    let listed = fixture.wt(["list", "--porcelain"]);
    assert_success(&listed);
    assert_eq!(
        String::from_utf8(listed.stdout).unwrap(),
        format!("-\tfeat/local-work\t{}\n", expected.display())
    );
    assert_success(&fixture.wt(["remove", "feat/local-work"]));
    assert!(!expected.exists());
    assert_eq!(
        git(&fixture.repo, ["branch", "--list", "feat/local-work"]),
        "feat/local-work"
    );
}

#[test]
fn add_tracks_an_existing_remote_branch() {
    let fixture = Fixture::new();
    command(&fixture.repo, "git", ["checkout", "-b", "feat/shared"]);
    fixture.write("shared.txt", "from remote\n");
    command(&fixture.repo, "git", ["add", "shared.txt"]);
    command(&fixture.repo, "git", ["commit", "-m", "shared work"]);
    command(
        &fixture.repo,
        "git",
        ["push", "-u", "origin", "feat/shared"],
    );
    command(&fixture.repo, "git", ["checkout", "main"]);
    command(&fixture.repo, "git", ["branch", "-D", "feat/shared"]);
    fixture.fail_gh_calls();

    let added = fixture.wt(["add", "feat/shared"]);

    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join("shared.txt")).unwrap(),
        "from remote\n"
    );
    assert_eq!(
        git(&path, ["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feat/shared"
    );
}

#[test]
fn add_starts_from_the_local_base_branch_when_it_is_ahead() {
    let fixture = Fixture::new();
    fixture.write("local-setup.txt", "ready\n");
    command(&fixture.repo, "git", ["add", "local-setup.txt"]);
    command(&fixture.repo, "git", ["commit", "-m", "local setup"]);
    let fetch_head = fixture.repo.join(".git/FETCH_HEAD");
    assert!(!fetch_head.exists());

    let output = fixture.wt(["add", "42"]);

    assert_success(&output);
    assert!(!fetch_head.exists());
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join("local-setup.txt")).unwrap(),
        "ready\n"
    );
}

#[test]
fn add_fetches_a_base_branch_that_is_not_local() {
    let fixture = Fixture::new();
    command(&fixture.repo, "git", ["checkout", "-b", "remote-only"]);
    fixture.write("remote-only.txt", "ready\n");
    command(&fixture.repo, "git", ["add", "remote-only.txt"]);
    command(&fixture.repo, "git", ["commit", "-m", "remote base"]);
    command(&fixture.repo, "git", ["push", "origin", "remote-only"]);
    command(&fixture.repo, "git", ["checkout", "main"]);
    command(&fixture.repo, "git", ["branch", "-D", "remote-only"]);
    fixture.write(".wtconfig", "[wt]\n\tbase = remote-only\n");

    let output = fixture.wt(["add", "42"]);

    assert_success(&output);
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join("remote-only.txt")).unwrap(),
        "ready\n"
    );
}

#[test]
fn existing_add_does_not_call_gh() {
    let fixture = Fixture::new();
    let first = fixture.wt(["add", "42"]);
    assert_success(&first);
    fixture.fail_gh_calls();

    let second = fixture.wt(["add", "42"]);

    assert_success(&second);
    assert_eq!(second.stdout, first.stdout);
}

#[test]
fn existing_state_matches_repository_names_case_insensitively() {
    let fixture = Fixture::new();
    let first = fixture.wt(["add", "42"]);
    assert_success(&first);
    let original = fixture.state.join("records/acme--example--42.json");
    let record = fs::read_to_string(&original)
        .unwrap()
        .replace("acme/example", "Acme/Example");
    fs::write(&original, record).unwrap();
    fixture.fail_gh_calls();

    let reused = fixture.wt(["add", "https://github.com/ACME/EXAMPLE/issues/42"]);
    assert_success(&reused);
    assert_success(&fixture.wt(["remove", "42"]));
    assert!(!original.exists());
}

#[test]
fn configured_add_only_reads_the_issue_from_gh() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    fixture.fail_gh_repo_calls();

    assert_success(&fixture.wt(["add", "42"]));
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
fn process_port_creates_managed_wt_env_without_a_primary_env_file() {
    let fixture = Fixture::new();
    let port = unused_port();
    fixture.write(".wtconfig", &format!("[wt]\n\tport = PORT:{port}\n"));

    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    let contents = fs::read_to_string(path.join(".wt.env")).unwrap();

    assert_eq!(env_value(&contents, "PORT"), port.to_string());
    assert_success(&fixture.wt(["remove", "42"]));
    assert!(!path.exists());
}

#[test]
fn env_and_process_variables_for_the_same_service_share_one_lease() {
    let fixture = Fixture::new();
    let port = unused_port();
    fixture.write(
        ".wtconfig",
        &format!("[wt]\n\tenv = .env\n\tport = WEB_PORT\n\tport = PORT:{port}\n"),
    );
    fixture.write(".env", &format!("WEB_PORT={port}\n"));

    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    let env = fs::read_to_string(path.join(".env")).unwrap();
    let process = fs::read_to_string(path.join(".wt.env")).unwrap();

    assert_eq!(env_value(&env, "WEB_PORT"), env_value(&process, "PORT"));
}

#[test]
fn init_with_process_port_installs_direnv_support() {
    let fixture = Fixture::new();

    let initialized = fixture.wt(["init", "--port", "PORT:3000", "--yes"]);

    assert_success(&initialized);
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".envrc")).unwrap(),
        "dotenv_if_exists .wt.env\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".gitignore")).unwrap(),
        "/.wt.env\n"
    );
}

#[test]
fn list_shows_and_remove_deletes_a_managed_worktree_but_keeps_its_branch() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["add", "42"]));
    let path = fixture.worktrees.join("example/fix-42-handle-empty-input");
    fixture.fail_gh_calls();

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
fn list_all_does_not_call_gh() {
    let fixture = Fixture::new();
    fixture.fail_gh_calls();

    assert_success(&fixture.wt(["list", "--all", "--porcelain"]));
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
fn bootstrap_logs_preserve_stdout_stderr_order_and_keep_stdout_path_only() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt([
        "init",
        "--bootstrap",
        "printf 'first\\n'; printf 'second\\n' >&2; printf 'third\\n'",
        "--yes",
    ]));
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    assert_eq!(
        String::from_utf8_lossy(&added.stderr),
        "first\nsecond\nthird\n"
    );
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    assert!(path.join(".git").exists());
}

#[test]
fn bootstrap_logs_are_visible_before_the_hook_exits() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt([
        "init",
        "--bootstrap",
        "printf 'waiting\\n'; read answer; test \"$answer\" = continue",
        "--yes",
    ]));
    let mut child = fixture
        .wt_command(["add", "42"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stderr).read_line(&mut line).unwrap();
        sender.send(line).unwrap();
    });
    let line = receiver.recv_timeout(Duration::from_secs(3));
    // Always release the hook, including when buffered output causes a timeout.
    writeln!(child.stdin.take().unwrap(), "continue").unwrap();
    assert_success(&child.wait_with_output().unwrap());
    reader.join().unwrap();
    assert_eq!(line.expect("bootstrap output was buffered"), "waiting\n");
}

#[test]
fn bootstrap_does_not_block_an_unrelated_add() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt([
        "init",
        "--bootstrap",
        "printf 'waiting\\n'; read answer",
        "--yes",
    ]));
    let mut child = fixture
        .wt_command(["add", "42"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    assert_eq!(line, "waiting\n");
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let fixture = &fixture;
        scope.spawn(move || {
            sender
                .send(fixture.wt(["add", "41", "--no-bootstrap"]))
                .unwrap()
        });
        let unrelated = receiver.recv_timeout(Duration::from_secs(3));
        writeln!(child.stdin.take().unwrap(), "continue").unwrap();
        assert_success(&child.wait_with_output().unwrap());
        assert_success(&unrelated.expect("unrelated add waited for bootstrap"));
    });
}

#[test]
fn same_worktree_add_and_remove_wait_for_bootstrap() {
    for args in [vec!["add", "42"], vec!["remove", "42", "--skip-teardown"]] {
        let fixture = Fixture::new();
        assert_success(&fixture.wt([
            "init",
            "--bootstrap",
            "printf 'waiting\\n'; read answer",
            "--yes",
        ]));
        let mut child = fixture
            .wt_command(["add", "42"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut line = String::new();
        stderr.read_line(&mut line).unwrap();
        assert_eq!(line, "waiting\n");
        let mut other = fixture
            .wt_command(std::iter::empty::<&str>())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let premature = other.try_wait().unwrap();
        writeln!(child.stdin.take().unwrap(), "continue").unwrap();
        assert_success(&child.wait_with_output().unwrap());
        let output = other.wait_with_output().unwrap();
        assert!(
            premature.is_none(),
            "same-worktree operation bypassed bootstrap lock"
        );
        assert_success(&output);
    }
}

#[test]
fn failed_bootstrap_streams_diagnostics_and_keeps_the_worktree() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt([
        "init",
        "--bootstrap",
        "printf 'broken\\n' >&2; exit 7",
        "--yes",
    ]));
    let failed = fixture.wt(["add", "42"]);
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("broken\n"));
    assert!(stderr.contains("exit status: 7"));
    assert!(stderr.contains("worktree kept at"));
    assert!(failed.stdout.is_empty());
    assert!(
        fixture
            .worktrees
            .join("example/fix-42-handle-empty-input/.git")
            .exists()
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
fn changing_non_command_config_does_not_revoke_command_trust() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["init", "--bootstrap", "printf safe > .safe", "--yes"]));
    fixture.write(".env", "READY=true\n");
    fixture.write(
        ".wtconfig",
        "[wt]\n\tbase = main\n\tenv = .env\n\tbootstrap = \"printf safe > .safe\"\n",
    );

    let output = fixture.wt(["add", "42"]);

    assert_success(&output);
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(fs::read_to_string(path.join(".safe")).unwrap(), "safe");
    assert_eq!(
        fs::read_to_string(path.join(".env")).unwrap(),
        "READY=true\n"
    );
}

#[test]
fn trust_is_hidden_from_normal_help() {
    let fixture = Fixture::new();

    let output = fixture.wt(["--help"]);

    assert_success(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("  trust"));
}

#[test]
fn trust_accepts_a_reviewed_config_without_rewriting_it() {
    let fixture = Fixture::new();
    let config = "[wt]\n\tbase = main\n\tbootstrap = \"printf reviewed > .reviewed\"\n";
    fixture.write(".wtconfig", config);
    fixture.fail_gh_calls();

    assert_success(&fixture.wt(["trust", "--yes"]));
    assert_eq!(
        fs::read_to_string(fixture.repo.join(".wtconfig")).unwrap(),
        config
    );
    write_fake_gh(&fixture.bin);
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join(".reviewed")).unwrap(),
        "reviewed"
    );
}

#[test]
fn command_trust_matches_repository_names_case_insensitively() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["init", "--bootstrap", "printf ready", "--yes"]));
    let trust = fixture.state.join("trust/acme--example");
    fs::rename(&trust, fixture.state.join("trust/Acme--Example")).unwrap();
    fixture.fail_gh_repo_calls();

    assert_success(&fixture.wt(["add", "42"]));
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
fn disposable_directory_cannot_contain_tracked_files() {
    let fixture = Fixture::new();
    fixture.write("generated/tracked.txt", "keep\n");
    command(&fixture.repo, "git", ["add", "generated/tracked.txt"]);
    command(
        &fixture.repo,
        "git",
        ["commit", "-m", "tracked generated file"],
    );
    fixture.write(".wtconfig", "[wt]\n\tdisposable = generated\n");

    let output = fixture.wt(["add", "42"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("generated/tracked.txt"));
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
fn remove_allows_ignored_empty_directory_trees() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "uploads/\n");
    command(&fixture.repo, "git", ["add", ".gitignore"]);
    command(&fixture.repo, "git", ["commit", "-m", "ignore uploads"]);
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    fs::create_dir_all(path.join("uploads/avatars/global")).unwrap();
    assert!(git(&path, ["status", "--porcelain"]).is_empty());
    assert!(git(&path, ["status", "--porcelain", "--ignored=matching"]).contains("!! uploads/"));

    assert_success(&fixture.wt(["remove", "42"]));
    assert!(!path.exists());
    assert!(fixture.wt(["list", "--porcelain"]).stdout.is_empty());
    assert_eq!(
        git(
            &fixture.repo,
            ["branch", "--list", "fix/42-handle-empty-input"]
        ),
        "fix/42-handle-empty-input"
    );
}

#[cfg(unix)]
#[test]
fn remove_protects_contents_of_ignored_directories() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let fixture = Fixture::new();
    fixture.write(".gitignore", "uploads/\n");
    command(&fixture.repo, "git", ["add", ".gitignore"]);
    command(&fixture.repo, "git", ["commit", "-m", "ignore uploads"]);
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    fs::create_dir_all(path.join("uploads/nested")).unwrap();
    let entry = path.join("uploads/nested/item");
    fs::write(&entry, "keep").unwrap();
    assert!(!fixture.wt(["remove", "42"]).status.success());
    assert_eq!(fs::read_to_string(&entry).unwrap(), "keep");
    fs::remove_file(&entry).unwrap();
    symlink("missing", &entry).unwrap();
    assert!(!fixture.wt(["remove", "42"]).status.success());
    assert!(fs::symlink_metadata(&entry).unwrap().is_symlink());
    fs::remove_file(&entry).unwrap();
    // Bind at a short path, then move the socket into the nested worktree.
    let socket_dir = tempfile::tempdir().unwrap();
    let bound_socket = socket_dir.path().join("s");
    let _listener = UnixListener::bind(&bound_socket).unwrap();
    let socket = path.join("uploads/s");
    fs::rename(bound_socket, &socket).unwrap();
    assert!(!fixture.wt(["remove", "42"]).status.success());
    assert!(socket.exists());
}

#[test]
fn list_reports_current_branch_without_changing_managed_identity() {
    let fixture = Fixture::new();
    let added = fixture.wt(["add", "original"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    command(&path, "git", ["switch", "-c", "changed"]);
    fixture.fail_gh_calls();
    for args in [vec!["list"], vec!["list", "--all"]] {
        let output = fixture.wt_command(args).output().unwrap();
        assert_success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains("changed (managed: original)"));
    }
    assert_eq!(
        String::from_utf8(fixture.wt(["list", "--porcelain"]).stdout).unwrap(),
        format!("-\toriginal\t{}\n", path.display())
    );
    assert_eq!(fixture.complete(&["remove", "orig"]), ["original"]);
    assert_success(&fixture.wt(["remove", "original"]));
    for branch in ["original", "changed"] {
        assert_eq!(git(&fixture.repo, ["branch", "--list", branch]), branch);
    }
}

#[test]
fn list_labels_detached_missing_and_unavailable_worktrees() {
    let fixture = Fixture::new();
    let added = fixture.wt(["add", "original"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    command(&path, "git", ["switch", "--detach"]);
    let listed = fixture.wt(["list"]);
    assert_success(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("(detached) (managed: original)"));
    fs::remove_dir_all(&path).unwrap();
    let listed = fixture.wt(["list"]);
    assert_success(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("(missing) (managed: original)"));
    fs::create_dir_all(&path).unwrap();
    let listed = fixture.wt(["list"]);
    assert_success(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("(unavailable) (managed: original)"));
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

#[test]
fn remove_cleans_nested_disposable_paths_and_keeps_unrelated_worktrees() {
    let fixture = Fixture::new();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tbase = main\n\tdisposable = generated/cache\n\tdisposable = generated\n\tdisposable = generated\n\tdisposable = absent\n",
    );
    let first = fixture.wt(["add", "ösalkdfjöalsk"]);
    assert_success(&first);
    let path = PathBuf::from(String::from_utf8(first.stdout).unwrap().trim());
    let second = fixture.wt(["add", "keep"]);
    assert_success(&second);
    let other = PathBuf::from(String::from_utf8(second.stdout).unwrap().trim());
    fs::create_dir_all(path.join("generated/cache/package/lib")).unwrap();
    fs::write(path.join("generated/cache/package/lib/index.js"), "cached").unwrap();

    let removed = fixture.wt(["remove", "ösalkdfjöalsk"]);

    assert_success(&removed);
    assert!(removed.stdout.is_empty());
    assert!(!path.exists());
    assert!(other.join("README.md").exists());
    assert_eq!(
        git(&fixture.repo, ["branch", "--list", "ösalkdfjöalsk"]),
        "ösalkdfjöalsk"
    );
}

#[test]
fn remove_refuses_tracked_changes_and_unknown_ignored_files() {
    for tracked in [false, true] {
        let fixture = Fixture::new();
        fixture.write(".gitignore", "private-notes\n");
        command(&fixture.repo, "git", ["add", ".gitignore"]);
        command(
            &fixture.repo,
            "git",
            ["commit", "-m", "ignore private notes"],
        );
        let added = fixture.wt(["add", "42"]);
        assert_success(&added);
        let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
        let file = if tracked {
            "README.md"
        } else {
            "private-notes"
        };
        fs::write(path.join(file), "keep this").unwrap();

        let removed = fixture.wt(["remove", "42"]);

        assert!(!removed.status.success());
        assert!(
            String::from_utf8_lossy(&removed.stderr).contains(if tracked {
                "tracked changes"
            } else {
                "unmanaged file"
            })
        );
        assert_eq!(fs::read_to_string(path.join(file)).unwrap(), "keep this");
        assert!(
            String::from_utf8_lossy(&fixture.wt(["list"]).stdout)
                .contains("fix/42-handle-empty-input")
        );
    }
}

#[cfg(unix)]
#[test]
fn cleanup_failure_keeps_worktree_and_record_and_reports_the_path() {
    use std::os::unix::fs::PermissionsExt;
    // Root bypasses the filesystem permission failure this test exercises.
    if Command::new("id").arg("-u").output().unwrap().stdout == b"0\n" {
        return;
    }
    let fixture = Fixture::new();
    assert_success(&fixture.wt([
        "init",
        "--disposable",
        "generated",
        "--teardown",
        "chmod 500 generated/package/lib",
        "--yes",
    ]));
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    let directory = path.join("generated/package/lib");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("cache"), "cached").unwrap();

    let removed = fixture.wt(["remove", "42"]);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(!removed.status.success());
    let stderr = String::from_utf8_lossy(&removed.stderr);
    assert!(stderr.contains("generated/package/lib"), "{stderr}");
    assert!(!stderr.contains("Worktree removed"));
    assert!(path.join(".git").exists());
    assert!(
        String::from_utf8_lossy(&fixture.wt(["list"]).stdout).contains("fix/42-handle-empty-input")
    );
    assert_success(&fixture.wt(["remove", "42", "--skip-teardown"]));
    assert!(!path.exists());
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
            [
                "config",
                &format!("url.{}.insteadOf", remote.display()),
                "https://github.com/acme/example.git",
            ],
        );
        command(
            &repo,
            "git",
            [
                "remote",
                "add",
                "origin",
                "https://github.com/acme/example.git",
            ],
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
        self.wt_command(args).output().unwrap()
    }

    fn completion_command(&self, words: &[&str]) -> Command {
        let mut command = self.wt_command(["--", "wt"].into_iter().chain(words.iter().copied()));
        command
            .env("COMPLETE", "bash")
            .env("_CLAP_COMPLETE_INDEX", words.len().to_string())
            .env("_CLAP_IFS", "\n");
        command
    }

    fn complete(&self, words: &[&str]) -> Vec<String> {
        let output = self.completion_command(words).output().unwrap();
        assert_success(&output);
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn wt_command<'a>(&self, args: impl IntoIterator<Item = &'a str>) -> Command {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        let mut command = Command::new(env!("CARGO_BIN_EXE_wt"));
        command
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", path)
            .env("WT_WORKTREE_ROOT", &self.worktrees)
            .env("WT_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config);
        command
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.repo.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn fail_gh_calls(&self) {
        fs::write(self.bin.join("gh"), "#!/bin/sh\nexit 99\n").unwrap();
    }

    fn fail_gh_repo_calls(&self) {
        fs::write(
            self.bin.join("gh"),
            "#!/bin/sh\nif [ \"$1\" = repo ]; then exit 99; fi\nprintf '{\"number\":%s,\"title\":\"Handle empty input\",\"state\":\"OPEN\",\"labels\":[{\"name\":\"bug\"}]}\\n' \"$3\"\n",
        )
        .unwrap();
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
        if let Ok(first) = TcpListener::bind(("127.0.0.1", port)) {
            if TcpListener::bind(("127.0.0.1", port + 1)).is_ok()
                && TcpListener::bind(("127.0.0.1", port + 2)).is_ok()
            {
                return (first, port);
            }
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
