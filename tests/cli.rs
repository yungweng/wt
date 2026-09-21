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
    let url = "https://github.com/acme/example/issues/42";
    assert!(String::from_utf8_lossy(&output.stderr).contains(url));
    fixture.fail_gh_calls();
    let existing = fixture.wt(["add", "42"]);
    assert_success(&existing);
    assert!(String::from_utf8_lossy(&existing.stderr).contains(url));
}

#[cfg(unix)]
#[test]
fn shell_function_changes_into_the_added_worktree_only_at_a_terminal() {
    let fixture = Fixture::new();
    // A terminal would otherwise offer the setup wizard and wait for input.
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    fixture.fail_gh_calls();
    let bin = Path::new(env!("CARGO_BIN_EXE_wt")).parent().unwrap();
    let path = format!(
        "{}:{}:{}",
        fixture.bin.display(),
        bin.display(),
        std::env::var("PATH").unwrap()
    );
    let setup_posix = |shell: &str| format!("eval \"$(wt shell {shell})\"");
    let body = |setup: String, status: &str, assign: &str| {
        format!(
            "{setup}\n\
             cd \"$REPO\"; {assign}; printf '%s\\n' \"$substituted\" > \"$OUT/substituted\"; pwd > \"$OUT/after-substitution\"\n\
             wt -v add feat/shell; pwd > \"$OUT/interactive\"\n\
             cd \"$REPO\"; wt add 'bad..name'; echo {status} > \"$OUT/failed\"; pwd > \"$OUT/after-failure\"\n\
             wt add --help > \"$OUT/help\"\n"
        )
    };
    let posix_assign = "substituted=$(wt add feat/shell)";
    let mut shells = vec![
        ("bash", body(setup_posix("bash"), "$?", posix_assign)),
        ("/bin/bash", body(setup_posix("bash"), "$?", posix_assign)),
        ("zsh", body(setup_posix("zsh"), "$?", posix_assign)),
        (
            "fish",
            body(
                "wt shell fish | source".to_owned(),
                "$status",
                "set substituted (wt add feat/shell)",
            ),
        ),
    ];
    shells.retain(|(shell, _)| {
        Command::new(shell)
            .args(["-c", "true"])
            .output()
            .is_ok_and(|output| output.status.success())
    });
    assert!(!shells.is_empty());
    let repo = fs::canonicalize(&fixture.repo).unwrap();
    for (shell, script) in shells {
        let out = tempfile::tempdir().unwrap();
        let file = out.path().join("script");
        fs::write(&file, script).unwrap();
        // `script` gives the shell a terminal on stdout, as in interactive use.
        let mut command = Command::new("script");
        if cfg!(target_os = "macos") {
            command.args(["-q", "/dev/null", shell, file.to_str().unwrap()]);
        } else {
            command.args(["-qec", &format!("{shell} {}", file.display()), "/dev/null"]);
        }
        let output = fixture
            .wt_command([])
            .get_envs()
            .fold(command, |mut command, (key, value)| {
                if let Some(value) = value {
                    command.env(key, value);
                }
                command
            })
            .env("PATH", &path)
            .env("REPO", &repo)
            .env("OUT", out.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_success(&output);
        let read = |name: &str| {
            fs::read_to_string(out.path().join(name))
                .unwrap_or_else(|_| panic!("{shell}: {name} missing: {output:?}"))
                .trim()
                .to_owned()
        };
        let worktree = fs::canonicalize(read("substituted")).unwrap();
        assert!(worktree.ends_with("example/feat-shell"), "{shell}");
        assert_eq!(
            PathBuf::from(read("after-substitution")),
            repo,
            "{shell}: substitution must not change directory"
        );
        assert_eq!(
            fs::canonicalize(read("interactive")).unwrap(),
            worktree,
            "{shell}"
        );
        assert_ne!(read("failed"), "0", "{shell}: failure status");
        assert_eq!(PathBuf::from(read("after-failure")), repo, "{shell}");
        assert!(read("help").contains("Create a worktree"), "{shell}");
    }
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
    assert!(!String::from_utf8_lossy(&added.stderr).contains("https://"));
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
fn add_opens_a_pull_request_on_its_head_branch() {
    let fixture = Fixture::new();
    command(&fixture.repo, "git", ["checkout", "-b", "feat/shared"]);
    fixture.write("shared.txt", "from pull request\n");
    command(&fixture.repo, "git", ["add", "shared.txt"]);
    command(&fixture.repo, "git", ["commit", "-m", "shared work"]);
    command(
        &fixture.repo,
        "git",
        ["push", "-u", "origin", "feat/shared"],
    );
    command(&fixture.repo, "git", ["checkout", "main"]);
    command(&fixture.repo, "git", ["branch", "-D", "feat/shared"]);
    // Without a local remote-tracking ref, only a fetch can find the head.
    command(
        &fixture.repo,
        "git",
        ["update-ref", "-d", "refs/remotes/origin/feat/shared"],
    );
    fixture.fake_pull_request("feat/shared", false);

    let added = fixture.wt(["add", "https://github.com/acme/example/pull/7"]);

    assert_success(&added);
    let expected = fixture.worktrees.join("example/feat-shared");
    assert_eq!(
        String::from_utf8(added.stdout.clone()).unwrap(),
        format!("{}\n", expected.display())
    );
    assert_eq!(
        fs::read_to_string(expected.join("shared.txt")).unwrap(),
        "from pull request\n"
    );
    assert_eq!(
        git(&expected, ["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feat/shared"
    );
    assert!(
        String::from_utf8_lossy(&added.stderr).contains("https://github.com/acme/example/pull/7")
    );
    fixture.fail_gh_calls();
    let reused = fixture.wt(["add", "7"]);
    assert_success(&reused);
    assert_eq!(reused.stdout, added.stdout);
}

#[test]
fn add_rejects_fork_pull_requests() {
    let fixture = Fixture::new();
    fixture.fake_pull_request("feat/shared", true);

    let added = fixture.wt(["add", "8"]);

    assert!(!added.status.success());
    assert!(String::from_utf8_lossy(&added.stderr).contains("gh pr checkout 8"));
    assert!(!fixture.worktrees.exists());
}

#[test]
fn add_reuses_the_issue_worktree_of_a_pull_request_head() {
    let fixture = Fixture::new();
    let issue = fixture.wt(["add", "42"]);
    assert_success(&issue);
    // The head is not on origin, so reuse must happen before any fetch.
    fixture.fake_pull_request("fix/42-handle-empty-input", false);

    let pull = fixture.wt(["add", "7"]);

    assert_success(&pull);
    assert_eq!(pull.stdout, issue.stdout);
    assert!(!fixture.state.join("records/acme--example--7.json").exists());
}

#[test]
fn add_starts_from_origin_even_when_the_local_base_branch_is_stale() {
    let fixture = Fixture::new();
    // origin/main moves ahead of the local main branch.
    let other = fixture.clone_repo("other");
    fs::write(other.join("upstream.txt"), "from origin\n").unwrap();
    command(&other, "git", ["add", "upstream.txt"]);
    command(&other, "git", ["commit", "-m", "upstream work"]);
    command(&other, "git", ["push", "origin", "main"]);
    // The local main branch has a commit that is not on origin.
    fixture.write("local-only.txt", "local\n");
    command(&fixture.repo, "git", ["add", "local-only.txt"]);
    command(&fixture.repo, "git", ["commit", "-m", "local only"]);

    let output = fixture.wt(["add", "42"]);

    assert_success(&output);
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join("upstream.txt")).unwrap(),
        "from origin\n"
    );
    assert!(!path.join("local-only.txt").exists());
    assert_eq!(
        git(&path, ["rev-parse", "HEAD"]),
        git(&fixture.repo, ["rev-parse", "origin/main"])
    );
}

#[test]
fn add_falls_back_to_a_local_base_branch_missing_on_origin() {
    let fixture = Fixture::new();
    command(&fixture.repo, "git", ["checkout", "-b", "local-base"]);
    fixture.write("local-base.txt", "ready\n");
    command(&fixture.repo, "git", ["add", "local-base.txt"]);
    command(&fixture.repo, "git", ["commit", "-m", "local base"]);
    command(&fixture.repo, "git", ["checkout", "main"]);
    fixture.write(".wtconfig", "[wt]\n\tbase = local-base\n");

    let output = fixture.wt(["add", "42"]);

    assert_success(&output);
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join("local-base.txt")).unwrap(),
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

    let readable = fixture.wt(["list"]);
    assert_success(&readable);
    let readable = String::from_utf8(readable.stdout).unwrap();
    assert!(readable.starts_with("acme/example\nISSUE"));
    assert!(readable.contains(" | BRANCH"));
    assert!(readable.contains(" | PATH"));
    assert!(readable.contains("#42"));
    assert!(readable.contains("fix/42-handle-empty-input"));
    assert!(!readable.contains('\t'));
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
fn add_copies_ignored_local_files_without_config() {
    let fixture = Fixture::new();
    fixture.write(
        ".git/info/exclude",
        "CLAUDE.local.md\nsettings.local.json\nnode_modules/\n",
    );
    fixture.write("CLAUDE.local.md", "notes\n");
    fixture.write(".claude/settings.local.json", "{}\n");
    fixture.write("node_modules/pkg/index.local.js", "vendored\n");
    fixture.write("unignored.local", "wip\n");
    fs::create_dir_all(fixture.repo.join("linked")).unwrap();
    std::os::unix::fs::symlink(
        "../CLAUDE.local.md",
        fixture.repo.join("linked/CLAUDE.local.md"),
    )
    .unwrap();

    let added = fixture.wt(["add", "42"]);
    assert_success(&added);

    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    assert_eq!(
        fs::read_to_string(path.join("CLAUDE.local.md")).unwrap(),
        "notes\n"
    );
    assert_eq!(
        fs::read_to_string(path.join(".claude/settings.local.json")).unwrap(),
        "{}\n"
    );
    assert!(!path.join("node_modules").exists());
    assert!(!path.join("unignored.local").exists());
    assert!(!path.join("linked/CLAUDE.local.md").is_symlink());
    assert_eq!(
        fs::read_to_string(path.join("linked/CLAUDE.local.md")).unwrap(),
        "notes\n"
    );
    assert_success(&fixture.wt(["remove", "42"]));
    assert!(!path.exists());
}

#[test]
fn add_copies_directories_globs_and_shared_symlinks() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let original_port = occupied.local_addr().unwrap().port();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tenv = .env\n\tcopy = certs\n\tcopy = \"**/*.pem\"\n\tport = APP_PORT\n",
    );
    fixture.write(".git/info/exclude", "*.local.md\ncerts/\n");
    fixture.write(".env", &format!("APP_PORT={original_port}\n"));
    fixture.write("certs/server.key", "key\n");
    fixture.write(
        "certs/nested/ca.conf",
        &format!("issuer=http://localhost:{original_port}\n"),
    );
    fixture.write("keys/one.pem", "pem\n");
    fixture.write("keys/deep/two.pem", "pem\n");
    fixture.write("keys/skip.txt", "text\n");
    let shared = fixture._temp.path().join("shared-notes.md");
    fs::write(&shared, "shared\n").unwrap();
    std::os::unix::fs::symlink(&shared, fixture.repo.join("notes.local.md")).unwrap();

    let added = fixture.wt(["add", "42"]);
    assert_success(&added);

    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    let assigned = env_value(&fs::read_to_string(path.join(".env")).unwrap(), "APP_PORT")
        .parse::<u16>()
        .unwrap();
    assert_ne!(assigned, original_port);
    assert_eq!(
        fs::read_to_string(path.join("certs/server.key")).unwrap(),
        "key\n"
    );
    assert_eq!(
        fs::read_to_string(path.join("certs/nested/ca.conf")).unwrap(),
        format!("issuer=http://localhost:{assigned}\n")
    );
    assert!(path.join("keys/one.pem").is_file());
    assert!(path.join("keys/deep/two.pem").is_file());
    assert!(!path.join("keys/skip.txt").exists());
    assert!(path.join("notes.local.md").is_symlink());
    assert_eq!(fs::read_link(path.join("notes.local.md")).unwrap(), shared);
    assert_eq!(fs::read_to_string(shared).unwrap(), "shared\n");

    assert_success(&fixture.wt(["remove", "42"]));
    assert!(!path.exists());
}

#[test]
fn wt_env_lists_env_ports_when_envrc_loads_it_and_direnv_allows_the_worktree() {
    let fixture = Fixture::new();
    let original_port = unused_port();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tenv = .env\n\tcompose = true\n\tport = APP_PORT\n",
    );
    fixture.write(".env", &format!("APP_PORT={original_port}\n"));
    fixture.write(".gitignore", "/.env\n/.wt.env\n");
    fixture.write(".envrc", "dotenv_if_exists .wt.env\n");
    command(&fixture.repo, "git", ["add", ".gitignore", ".envrc"]);
    command(&fixture.repo, "git", ["commit", "-m", "direnv"]);
    command(&fixture.repo, "git", ["push", "origin", "main"]);
    fixture.fake_direnv(&["example"]);

    let added = fixture.wt(["add", "42"]);
    assert_success(&added);

    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    let process = fs::read_to_string(path.join(".wt.env")).unwrap();
    let env = fs::read_to_string(path.join(".env")).unwrap();
    assert_eq!(env_value(&process, "APP_PORT"), env_value(&env, "APP_PORT"));
    assert_eq!(
        env_value(&process, "COMPOSE_PROJECT_NAME"),
        "wt-42-acme-example"
    );
    let allowed = fs::read_to_string(fixture.bin.join("direnv-allow.log")).unwrap();
    assert_eq!(
        fs::canonicalize(allowed.trim()).unwrap(),
        fs::canonicalize(&path).unwrap()
    );

    // A checkout whose .envrc direnv does not allow never allows the worktree.
    fixture.fake_direnv(&[]);
    fs::remove_file(fixture.bin.join("direnv-allow.log")).unwrap();
    assert_success(&fixture.wt(["add", "43"]));
    assert!(!fixture.bin.join("direnv-allow.log").exists());
}

#[test]
fn bootstrap_generated_copy_paths_are_rewritten_and_managed() {
    let fixture = Fixture::new();
    let original_port = unused_port();
    fixture.write(".env", &format!("APP_PORT={original_port}\n"));
    let initialized = fixture.wt([
        "init",
        "--env",
        ".env",
        "--copy",
        "generated/app.conf",
        "--copy",
        "never.conf",
        "--port",
        "APP_PORT",
        "--bootstrap",
        &format!("mkdir -p generated && printf 'URL=http://localhost:{original_port}\\n' > generated/app.conf"),
        "--yes",
    ]);
    assert_success(&initialized);

    let added = fixture.wt(["add", "42"]);
    assert_success(&added);

    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    let env = fs::read_to_string(path.join(".env")).unwrap();
    let assigned = env_value(&env, "APP_PORT");
    assert_eq!(
        fs::read_to_string(path.join("generated/app.conf")).unwrap(),
        format!("URL=http://localhost:{assigned}\n")
    );
    assert!(
        String::from_utf8_lossy(&added.stderr).contains("never.conf"),
        "{:?}",
        added.stderr
    );
    let record = fs::read_to_string(fixture.state.join("records/acme--example--42.json")).unwrap();
    assert!(record.contains("generated/app.conf"), "{record}");

    assert_success(&fixture.wt(["remove", "42"]));
    assert!(!path.exists());
}

#[test]
fn remove_drops_the_record_of_a_missing_worktree() {
    let fixture = Fixture::new();
    let added = fixture.wt(["add", "feat/gone"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    command(
        &fixture.repo,
        "git",
        ["worktree", "remove", path.to_str().unwrap()],
    );

    assert_success(&fixture.wt(["remove", "feat/gone"]));

    let listed = String::from_utf8(fixture.wt(["list", "--porcelain"]).stdout).unwrap();
    assert!(!listed.contains("feat/gone"), "{listed}");
    assert!(!git(&fixture.repo, ["worktree", "list"]).contains("gone"));
    assert_eq!(
        git(&fixture.repo, ["branch", "--list", "feat/gone"]),
        "feat/gone"
    );
}

#[test]
fn doctor_reports_missing_private_files_and_shadowed_ports() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let original_port = occupied.local_addr().unwrap().port();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tenv = .env\n\tport = APP_PORT\n\tdisposable = node_modules\n",
    );
    fixture.write(
        ".git/info/exclude",
        ".env\nsecrets.json\nnode_modules/\ncerts/\n*.log\n",
    );
    fixture.write(".env", &format!("APP_PORT={original_port}\n"));
    fixture.write("secrets.json", "{}\n");
    fixture.write("node_modules/pkg/index.js", "cached\n");
    fixture.write("certs/server.key", "key\n");
    fixture.write("server.log", "noise\n");
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());

    let checked = fixture
        .wt_command(["doctor"])
        .current_dir(&path)
        .env("APP_PORT", original_port.to_string())
        .output()
        .unwrap();
    assert!(!checked.status.success());
    let text = String::from_utf8_lossy(&checked.stdout);
    assert!(text.contains("secrets.json"), "{text}");
    assert!(text.contains("certs/"), "{text}");
    assert!(!text.contains("node_modules"), "{text}");
    assert!(!text.contains("server.log"), "{text}");
    assert!(
        text.contains(&format!("APP_PORT={original_port} in this shell")),
        "{text}"
    );

    fs::write(path.join("secrets.json"), "{}\n").unwrap();
    fs::create_dir_all(path.join("certs")).unwrap();
    fs::write(path.join("certs/server.key"), "key\n").unwrap();
    let healthy = fixture
        .wt_command(["doctor"])
        .current_dir(&path)
        .env_remove("APP_PORT")
        .output()
        .unwrap();
    assert_success(&healthy);
    assert!(String::from_utf8_lossy(&healthy.stdout).contains("No problems found"));

    let outside = fixture.wt(["doctor"]);
    assert!(!outside.status.success());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("main checkout"));
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
        "first\nsecond\nthird\nhttps://github.com/acme/example/issues/42\n"
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
    fixture.write(".wtconfig", "[wt]\n\tenv = .env\n\tport = APP_PORT\n");
    fixture.write(".env", "OTHER=1\n");
    let path = fixture.worktrees.join("example/fix-42-handle-empty-input");

    let failed = fixture.wt(["add", "42"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("APP_PORT is missing from .env"));
    assert!(!path.exists());
    assert_eq!(
        git(
            &fixture.repo,
            ["branch", "--list", "fix/42-handle-empty-input"]
        ),
        "fix/42-handle-empty-input"
    );

    fixture.write(".env", "APP_PORT=3000\n");
    let retried = fixture.wt(["add", "42"]);
    assert_success(&retried);
    assert!(path.join(".env").exists());
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
    command(&fixture.repo, "git", ["push", "origin", "main"]);
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
    command(&fixture.repo, "git", ["push", "origin", "main"]);
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
    command(&fixture.repo, "git", ["push", "origin", "main"]);
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
        "chmod 500 .",
        "--yes",
    ]));
    let added = fixture.wt(["add", "42"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    fs::create_dir_all(path.join("generated/package/lib")).unwrap();
    fs::write(path.join("generated/package/lib/cache"), "cached").unwrap();

    let removed = fixture.wt(["remove", "42"]);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(!removed.status.success());
    let stderr = String::from_utf8_lossy(&removed.stderr);
    assert!(stderr.contains("generated"), "{stderr}");
    assert!(!stderr.contains("Worktree removed"));
    assert!(path.join(".git").exists());
    assert!(path.join("generated/package/lib/cache").exists());
    assert!(
        String::from_utf8_lossy(&fixture.wt(["list"]).stdout).contains("fix/42-handle-empty-input")
    );
    assert_success(&fixture.wt(["remove", "42", "--skip-teardown"]));
    assert!(!path.exists());
}

#[test]
fn remove_moves_generated_files_aside_and_purges_them_in_the_background() {
    let fixture = Fixture::new();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tbase = main\n\tdisposable = generated\n",
    );
    let added = fixture.wt(["add", "feat/trash"]);
    assert_success(&added);
    let path = PathBuf::from(String::from_utf8(added.stdout).unwrap().trim());
    fs::create_dir_all(path.join("generated/package/lib")).unwrap();
    fs::write(path.join("generated/package/lib/index.js"), "cached").unwrap();

    assert_success(&fixture.wt(["remove", "feat/trash"]));

    assert!(!path.exists());
    let trash = fixture.worktrees.join("example/.wt-trash");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while trash.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!trash.exists(), "background purge left {}", trash.display());
    assert_eq!(
        git(&fixture.repo, ["branch", "--list", "feat/trash"]),
        "feat/trash"
    );
}

#[test]
fn purge_trash_ignores_directories_that_are_not_wt_trash() {
    let fixture = Fixture::new();
    let directory = fixture.worktrees.join("keep");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("file"), "keep").unwrap();
    assert_success(&fixture.wt(["purge-trash", directory.to_str().unwrap()]));
    assert!(directory.join("file").exists());
}

#[test]
fn clean_previews_then_removes_merged_worktrees_and_keeps_branches() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["add", "feat/merged"]));
    let path = fixture.worktrees.join("example/feat-merged");
    fs::write(path.join("feature"), "finished\n").unwrap();
    command(&path, "git", ["add", "feature"]);
    command(&path, "git", ["commit", "-m", "feature"]);
    let head = git(&path, ["rev-parse", "HEAD"]);
    command(&fixture.repo, "git", ["merge", "--ff-only", "feat/merged"]);

    let preview = fixture.wt(["clean", "--dry-run"]);
    assert_success(&preview);
    let text = String::from_utf8_lossy(&preview.stdout);
    assert!(
        text.contains("Ready to remove (1)") && text.contains("feat/merged"),
        "{text}"
    );
    assert!(text.contains("merged into main"), "{text}");
    assert!(path.join("feature").exists());
    let unconfirmed = fixture
        .wt_command(["clean"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!unconfirmed.status.success());
    assert!(String::from_utf8_lossy(&unconfirmed.stderr).contains("needs confirmation"));
    assert!(path.exists());

    assert_success(&fixture.wt(["clean", "--yes"]));
    assert!(!path.exists());
    assert_eq!(git(&fixture.repo, ["rev-parse", "feat/merged"]), head);
    assert!(fixture.wt(["list", "--porcelain"]).stdout.is_empty());
}

#[test]
fn clean_skips_locked_worktrees_before_teardown_or_generated_file_cleanup() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n\tdisposable = generated\n\tteardown = touch generated/teardown-ran\n");
    assert_success(&fixture.wt(["trust", "--yes"]));
    assert_success(&fixture.wt(["add", "feat/locked"]));
    let path = fixture.worktrees.join("example/feat-locked");
    fs::create_dir(path.join("generated")).unwrap();
    fs::write(path.join("generated/keep"), "cache\n").unwrap();
    command(
        &fixture.repo,
        "git",
        ["worktree", "lock", path.to_str().unwrap()],
    );

    let preview = fixture.wt(["clean", "--dry-run"]);
    assert_success(&preview);
    let text = String::from_utf8_lossy(&preview.stdout);
    assert!(
        text.contains("Skipped (1)") && text.contains("feat/locked"),
        "{text}"
    );
    assert!(text.contains("locked"), "{text}");
    assert_success(&fixture.wt(["clean", "--yes"]));
    assert!(path.join("generated/keep").exists());
    assert!(!path.join("generated/teardown-ran").exists());
}

#[test]
fn clean_without_saved_worktrees_does_not_create_state() {
    let fixture = Fixture::new();
    fixture.fail_gh_calls();
    for args in [["clean", "--dry-run"], ["clean", "--yes"]] {
        let output = fixture.wt(args);
        assert_success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains("No managed worktrees"));
        assert!(!fixture.state.exists());
    }
}

#[test]
fn clean_preserves_tracked_untracked_ignored_and_changed_copied_files() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n\tenv = .env\n");
    fixture.write(".env", "SECRET=original\n");
    fixture.write(".gitignore", "ignored\n.env\n.wtconfig\n");
    command(&fixture.repo, "git", ["add", ".gitignore"]);
    command(&fixture.repo, "git", ["commit", "-m", "ignore local files"]);
    for (branch, file, contents) in [
        ("tracked", "README.md", "unfinished tracked work\n"),
        ("untracked", "notes", "unfinished notes\n"),
        ("ignored", "ignored", "important ignored work\n"),
        ("copied", ".env", "SECRET=changed\n"),
    ] {
        assert_success(&fixture.wt(["add", branch]));
        fs::write(
            fixture.worktrees.join("example").join(branch).join(file),
            contents,
        )
        .unwrap();
    }
    assert_success(&fixture.wt(["add", "safe"]));
    // Unmerged local work is never offered for removal, even with --force.
    assert_success(&fixture.wt(["add", "unmerged"]));
    let unmerged = fixture.worktrees.join("example/unmerged");
    command(
        &unmerged,
        "git",
        ["commit", "--allow-empty", "-m", "unmerged"],
    );
    fs::write(unmerged.join("notes"), "unfinished\n").unwrap();
    fixture.merged_pull_requests(&[]);
    // Without their own commits or a merged PR, changed worktrees may be new
    // work in progress, so even --force keeps them.
    let output = fixture.wt(["clean", "--yes", "--force"]);
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    let skipped = text.split("Skipped (5)").nth(1).unwrap();
    for branch in ["tracked", "untracked", "ignored", "copied", "unmerged"] {
        assert!(skipped.contains(branch), "{text}");
        assert!(fixture.worktrees.join("example").join(branch).exists());
    }
    assert!(!fixture.worktrees.join("example/safe").exists());
    assert_eq!(
        fs::read_to_string(fixture.worktrees.join("example/copied/.env")).unwrap(),
        "SECRET=changed\n"
    );

    let head = git(&fixture.repo, ["rev-parse", "HEAD"]);
    fixture.merged_pull_requests(&[
        ("tracked", &head),
        ("untracked", &head),
        ("ignored", &head),
        ("copied", &head),
    ]);
    let output = fixture.wt(["clean", "--yes"]);
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    let forced = text.split("Needs --force (4)").nth(1).unwrap();
    for branch in ["tracked", "untracked", "ignored", "copied"] {
        assert!(forced.contains(branch), "{text}");
        assert!(fixture.worktrees.join("example").join(branch).exists());
    }
    assert!(text.contains("wt clean --force"), "{text}");
    assert!(
        text.split("Skipped (1)")
            .nth(1)
            .unwrap()
            .contains("unmerged"),
        "{text}"
    );

    let preview = fixture.wt(["clean", "--dry-run", "--force"]);
    assert_success(&preview);
    let text = String::from_utf8_lossy(&preview.stdout);
    assert!(text.contains("Ready to remove (4)"), "{text}");
    assert!(text.contains("Modified: README.md"), "{text}");
    assert!(text.contains("lose the local changes"), "{text}");

    assert_success(&fixture.wt(["clean", "--yes", "--force"]));
    for branch in ["tracked", "untracked", "ignored", "copied"] {
        assert!(!fixture.worktrees.join("example").join(branch).exists());
        assert_eq!(git(&fixture.repo, ["branch", "--list", branch]), branch);
    }
    assert_eq!(
        fs::read_to_string(unmerged.join("notes")).unwrap(),
        "unfinished\n"
    );
}

#[test]
fn forced_clean_keeps_changes_that_appear_after_the_preview() {
    let fixture = Fixture::new();
    fixture.write(
        ".wtconfig",
        "[wt]\n\tbase = main\n\tteardown = echo late > late-notes\n",
    );
    assert_success(&fixture.wt(["trust", "--yes"]));
    assert_success(&fixture.wt(["add", "dirty"]));
    let path = fixture.worktrees.join("example/dirty");
    fs::write(path.join("notes"), "shown in preview\n").unwrap();
    fixture.merged_pull_requests(&[("dirty", &git(&path, ["rev-parse", "HEAD"]))]);

    let output = fixture.wt(["clean", "--yes", "--force"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unmanaged file: late-notes"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(path.join("notes").exists());
}

#[test]
fn clean_rejects_unrelated_prs_and_newer_local_commits_after_squash_merge() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    assert_success(&fixture.wt(["add", "feat/squashed"]));
    let path = fixture.worktrees.join("example/feat-squashed");
    for message in ["first", "second"] {
        fs::write(path.join("feature"), message).unwrap();
        command(&path, "git", ["add", "feature"]);
        command(&path, "git", ["commit", "-m", message]);
    }
    let head = git(&path, ["rev-parse", "HEAD"]);
    command(&fixture.repo, "git", ["merge", "--squash", "feat/squashed"]);
    command(&fixture.repo, "git", ["commit", "-m", "squash feature"]);
    fixture.fail_gh_calls();
    let unavailable = fixture.wt(["clean", "--yes"]);
    assert_success(&unavailable);
    assert!(String::from_utf8_lossy(&unavailable.stdout).contains("cannot verify merged PRs"));
    assert!(path.exists());
    fs::write(
        fixture.bin.join("gh"),
        "#!/bin/sh\ncat \"${0%/*}/prs.json\"\n",
    )
    .unwrap();
    let merged = serde_json::json!({
        "number": 123, "state": "MERGED", "headRefName": "feat/squashed",
        "headRefOid": head, "baseRefName": "main", "isCrossRepository": false,
    });
    for (field, value) in [
        ("state", serde_json::json!("CLOSED")),
        ("headRefName", serde_json::json!("another-branch")),
        (
            "headRefOid",
            serde_json::json!(git(&fixture.repo, ["rev-parse", "HEAD"])),
        ),
        ("baseRefName", serde_json::json!("release")),
        ("isCrossRepository", serde_json::json!(true)),
    ] {
        let mut rejected = merged.clone();
        rejected[field] = value;
        fs::write(
            fixture.bin.join("prs.json"),
            serde_json::to_vec(&vec![rejected]).unwrap(),
        )
        .unwrap();
        let output = fixture.wt(["clean", "--yes"]);
        assert_success(&output);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Skipped (1)"),
            "{field}"
        );
        assert!(path.exists(), "{field}");
    }
    fs::write(
        fixture.bin.join("prs.json"),
        serde_json::to_vec(&vec![merged]).unwrap(),
    )
    .unwrap();
    let preview = fixture.wt(["clean", "--dry-run"]);
    assert_success(&preview);
    assert!(String::from_utf8_lossy(&preview.stdout).contains("merged PR #123 into main"));

    // Reusing the branch for more work must invalidate the old merged PR.
    command(
        &path,
        "git",
        ["commit", "--allow-empty", "-m", "new work after merge"],
    );
    assert_success(&fixture.wt(["clean", "--yes"]));
    assert!(path.exists());
    command(&path, "git", ["switch", "--detach", &head]);
    command(
        &fixture.repo,
        "git",
        ["branch", "-f", "feat/squashed", &head],
    );
    command(&path, "git", ["switch", "feat/squashed"]);
    assert_success(&fixture.wt(["clean", "--yes"]));
    assert!(!path.exists());
    assert_eq!(git(&fixture.repo, ["rev-parse", "feat/squashed"]), head);
}

#[test]
fn clean_skips_current_base_detached_switched_and_missing_worktrees() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    command(&fixture.repo, "git", ["switch", "-c", "caller"]);
    for branch in ["main", "current", "detached", "switched", "missing"] {
        assert_success(&fixture.wt(["add", branch]));
    }
    let root = fixture.worktrees.join("example");
    command(&root.join("detached"), "git", ["switch", "--detach"]);
    command(&root.join("switched"), "git", ["switch", "-c", "other"]);
    command(
        &fixture.repo,
        "git",
        ["worktree", "remove", root.join("missing").to_str().unwrap()],
    );
    fs::create_dir(root.join("current/subdirectory")).unwrap();
    let output = fixture
        .wt_command(["clean", "--yes"])
        .current_dir(root.join("current/subdirectory"))
        .output()
        .unwrap();
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    let skipped = text.split("Skipped (").nth(1).unwrap();
    for branch in ["main", "current", "detached", "switched"] {
        assert!(skipped.contains(branch), "{text}");
        assert!(root.join(branch).exists());
    }
    // A missing directory has nothing to lose, so no --force is needed.
    let ready = text.split("Ready to remove (1)").nth(1).unwrap();
    assert!(
        ready.split("Skipped (").next().unwrap().contains("missing"),
        "{text}"
    );
    assert!(!text.contains("Needs --force"), "{text}");
    assert!(text.contains("worktree path is missing"), "{text}");
    assert!(text.contains("current worktree"), "{text}");
    assert!(text.contains("base branch"), "{text}");
    let listed = String::from_utf8(fixture.wt(["list", "--porcelain"]).stdout).unwrap();
    assert!(!listed.contains("\tmissing\t"), "{listed}");
    assert!(!git(&fixture.repo, ["worktree", "list"]).contains("/missing "));
    for branch in ["main", "current", "detached", "switched"] {
        assert!(root.join(branch).exists());
    }
}

#[test]
fn clean_leaves_other_clones_and_repositories_alone() {
    let fixture = Fixture::new();
    assert_success(&fixture.wt(["add", "42"]));
    let path = fixture.worktrees.join("example/fix-42-handle-empty-input");
    let other = Fixture::new();
    let output = fixture
        .wt_command(["clean", "--yes"])
        .current_dir(&other.repo)
        .output()
        .unwrap();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("another clone"));
    assert!(path.exists());
    command(
        &other.repo,
        "git",
        [
            "remote",
            "set-url",
            "origin",
            "https://github.com/acme/other.git",
        ],
    );
    let output = fixture
        .wt_command(["clean", "--yes"])
        .current_dir(&other.repo)
        .output()
        .unwrap();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("No managed worktrees"));
    assert!(path.exists());
}

#[test]
fn clean_rechecks_after_teardown_and_continues_after_a_failed_candidate() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    command(
        &fixture.repo,
        "git",
        [
            "config",
            "-f",
            ".wtconfig",
            "wt.teardown",
            "if [ \"$(git branch --show-current)\" = a ]; then echo unfinished >> README.md; fi",
        ],
    );
    assert_success(&fixture.wt(["trust", "--yes"]));
    for branch in ["a", "b"] {
        assert_success(&fixture.wt(["add", branch]));
    }
    let root = fixture.worktrees.join("example");
    assert_success(&fixture.wt(["clean", "--dry-run"]));
    assert_eq!(
        fs::read_to_string(root.join("a/README.md")).unwrap(),
        "example\n"
    );
    let output = fixture.wt(["clean", "--yes"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("tracked changes"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(root.join("a/README.md"))
            .unwrap()
            .contains("unfinished")
    );
    assert!(!root.join("b").exists());
    let listed = fixture.wt(["list", "--porcelain"]);
    assert_success(&listed);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("\ta\t"));
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("\tb\t"));
    command(&root.join("a"), "git", ["restore", "README.md"]);
    assert_success(&fixture.wt(["clean", "--yes", "--skip-teardown"]));
    assert!(!root.join("a").exists());
}

#[test]
fn clean_keeps_a_candidate_whose_head_changes_after_preview_even_if_merged() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    command(
        &fixture.repo,
        "git",
        [
            "config",
            "-f",
            ".wtconfig",
            "wt.teardown",
            "if [ \"$(git branch --show-current)\" = a ]; then git -C ../b commit --allow-empty -m late; git -C \"$CLEAN_TEST_REPO\" merge --ff-only b; fi",
        ],
    );
    assert_success(&fixture.wt(["trust", "--yes"]));
    for branch in ["a", "b"] {
        assert_success(&fixture.wt(["add", branch]));
    }
    let root = fixture.worktrees.join("example");
    let before = git(&root.join("b"), ["rev-parse", "HEAD"]);
    let output = fixture
        // Serial order makes the teardown of `a` change `b` before its recheck.
        .wt_command(["clean", "--yes", "--verbose"])
        .env("CLEAN_TEST_REPO", &fixture.repo)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("HEAD changed after preview"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("a").exists());
    assert!(root.join("b").exists());
    let after = git(&root.join("b"), ["rev-parse", "HEAD"]);
    assert_ne!(before, after);
    assert_eq!(after, git(&fixture.repo, ["rev-parse", "main"]));
}

#[test]
fn clean_accepts_local_commits_included_in_a_later_merged_pr_head() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    assert_success(&fixture.wt(["add", "feat/behind"]));
    let path = fixture.worktrees.join("example/feat-behind");
    command(
        &path,
        "git",
        ["commit", "--allow-empty", "-m", "local work"],
    );
    let local = git(&path, ["rev-parse", "HEAD"]);
    command(
        &path,
        "git",
        ["commit", "--allow-empty", "-m", "included before merge"],
    );
    let merged = git(&path, ["rev-parse", "HEAD"]);
    command(&path, "git", ["switch", "--detach", &local]);
    command(
        &fixture.repo,
        "git",
        ["branch", "-f", "feat/behind", &local],
    );
    command(&path, "git", ["switch", "feat/behind"]);
    fs::write(
        fixture.bin.join("gh"),
        "#!/bin/sh\ncat \"${0%/*}/prs.json\"\n",
    )
    .unwrap();
    fs::write(
        fixture.bin.join("prs.json"),
        serde_json::to_vec(&serde_json::json!([{
            "number": 123, "state": "MERGED", "headRefName": "feat/behind",
            "headRefOid": merged, "baseRefName": "main", "isCrossRepository": false
        }]))
        .unwrap(),
    )
    .unwrap();
    let output = fixture.wt(["clean", "--dry-run"]);
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Ready to remove (1)"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_success(&fixture.wt(["clean", "--yes"]));
    assert!(!path.exists());
    assert_eq!(git(&fixture.repo, ["rev-parse", "feat/behind"]), local);
}

#[test]
fn clean_verifies_missing_pr_commits_on_github_without_fetching() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    assert_success(&fixture.wt(["add", "feat/remote"]));
    // `wt add` fetches the base; the checks below cover `clean` only.
    let fetch_head = fixture.repo.join(".git/FETCH_HEAD");
    let _ = fs::remove_file(&fetch_head);
    let path = fixture.worktrees.join("example/feat-remote");
    command(&path, "git", ["commit", "--allow-empty", "-m", "local"]);
    fs::write(fixture.bin.join("gh"), "#!/bin/sh\nif [ \"$1\" = api ]; then cat \"${0%/*}/comparison\"; else cat \"${0%/*}/prs.json\"; fi\n").unwrap();
    fs::write(
        fixture.bin.join("prs.json"),
        serde_json::to_vec(&serde_json::json!([{
            "number": 123, "state": "MERGED", "headRefName": "feat/remote",
            "headRefOid": "1111111111111111111111111111111111111111",
            "baseRefName": "main", "isCrossRepository": false
        }]))
        .unwrap(),
    )
    .unwrap();
    for status in ["behind", "diverged", "unexpected"] {
        fs::write(fixture.bin.join("comparison"), status).unwrap();
        let output = fixture.wt(["clean", "--yes"]);
        assert_success(&output);
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("Skipped (1)"), "{text}");
        assert!(text.contains("merged"), "{text}");
        assert!(path.exists());
    }
    for status in ["ahead", "identical"] {
        fs::write(fixture.bin.join("comparison"), status).unwrap();
        let output = fixture.wt(["clean", "--dry-run"]);
        assert_success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains("Ready to remove (1)"));
    }
    assert_success(&fixture.wt(["clean", "--yes"]));
    assert!(!path.exists());
    assert!(!fetch_head.exists());
}

#[test]
fn clean_shares_one_recent_pr_lookup_across_worktrees() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    for branch in ["one", "two"] {
        assert_success(&fixture.wt(["add", branch]));
        command(
            &fixture.worktrees.join("example").join(branch),
            "git",
            ["commit", "--allow-empty", "-m", branch],
        );
    }
    fs::write(
        fixture.bin.join("gh"),
        "#!/bin/sh\necho lookup >> \"${0%/*}/calls\"\nprintf '[]'\n",
    )
    .unwrap();
    let output = fixture.wt(["clean", "--dry-run"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Skipped (2)"));
    assert_eq!(
        fs::read_to_string(fixture.bin.join("calls")).unwrap(),
        "lookup\n"
    );
}

#[test]
fn clean_checks_older_branch_prs_when_the_recent_page_is_full() {
    let fixture = Fixture::new();
    fixture.write(".wtconfig", "[wt]\n\tbase = main\n");
    assert_success(&fixture.wt(["add", "old"]));
    let path = fixture.worktrees.join("example/old");
    command(&path, "git", ["commit", "--allow-empty", "-m", "old work"]);
    let pr = serde_json::json!({"number": 123, "state": "MERGED", "headRefName": "old",
        "headRefOid": git(&path, ["rev-parse", "HEAD"]), "baseRefName": "main", "isCrossRepository": false});
    let recent = (0..100)
        .map(|number| {
            let mut pr = pr.clone();
            pr["number"] = serde_json::json!(number);
            pr["headRefName"] = serde_json::json!("unrelated");
            pr
        })
        .collect::<Vec<_>>();
    fs::write(
        fixture.bin.join("recent.json"),
        serde_json::to_vec(&recent).unwrap(),
    )
    .unwrap();
    fs::write(
        fixture.bin.join("old.json"),
        serde_json::to_vec(&vec![pr]).unwrap(),
    )
    .unwrap();
    fs::write(fixture.bin.join("gh"), "#!/bin/sh\nfor arg do\nif [ \"$arg\" = --head ]; then cat \"${0%/*}/old.json\"; exit; fi\ndone\ncat \"${0%/*}/recent.json\"\n").unwrap();
    let output = fixture.wt(["clean", "--dry-run"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Ready to remove (1)"));
}

#[test]
fn tables_fit_narrow_terminals_and_align_unicode_branches() {
    let fixture = Fixture::new();
    let branch = "feat/füße-界界界-long-branch-name-for-a-narrow-terminal";
    assert_success(&fixture.wt(["add", branch]));
    for args in [vec!["list"], vec!["clean", "--dry-run"]] {
        let output = fixture
            .wt_command(args)
            .env("COLUMNS", "80")
            .output()
            .unwrap();
        assert_success(&output);
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            assert!(console::measure_text_width(line) <= 80, "{line}");
        }
        let lines = text
            .lines()
            .filter(|line| line.contains(" | "))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "{text}");
        let widths = |line: &str| {
            line.split(" | ")
                .take(2)
                .map(console::measure_text_width)
                .collect::<Vec<_>>()
        };
        assert_eq!(widths(lines[0]), widths(lines[1]));
        assert!(text.contains('…'));
    }
    let full = fixture.wt(["list", "--porcelain"]);
    assert_success(&full);
    assert!(String::from_utf8_lossy(&full.stdout).contains(branch));
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
            ["init", "--bare", "-b", "main", remote.to_str().unwrap()],
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

    /// Clones the shared remote into a sibling directory named `name`.
    fn clone_repo(&self, name: &str) -> PathBuf {
        let root = self.repo.parent().unwrap().to_path_buf();
        let remote = root.join("remote.git");
        let target = root.join(name);
        command(
            &root,
            "git",
            ["clone", remote.to_str().unwrap(), target.to_str().unwrap()],
        );
        command(&target, "git", ["config", "user.email", "dev@example.com"]);
        command(&target, "git", ["config", "user.name", "Example Dev"]);
        command(&target, "git", ["config", "commit.gpgsign", "false"]);
        target
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

    /// Installs a fake direnv that allows `.envrc` only in directories whose
    /// basename is listed, and logs every `allow` with the directory path.
    fn fake_direnv(&self, allowed: &[&str]) {
        let names = allowed.join(" ");
        let path = self.bin.join("direnv");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  status)\n    [ -f .envrc ] || exit 0\n    code=1\n    for name in {names}; do [ \"$name\" = \"$(basename \"$PWD\")\" ] && code=0; done\n    echo \"Found RC path $PWD/.envrc\"\n    echo \"Found RC allowed $code\" ;;\n  allow) echo \"$PWD\" >> \"${{0%/*}}/direnv-allow.log\" ;;\n  *) exit 99 ;;\nesac\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn fail_gh_calls(&self) {
        fs::write(self.bin.join("gh"), "#!/bin/sh\nexit 99\n").unwrap();
    }

    fn fail_gh_repo_calls(&self) {
        fs::write(
            self.bin.join("gh"),
            "#!/bin/sh\nif [ \"$1\" = repo ]; then exit 99; fi\nprintf '{\"number\":%s,\"title\":\"Handle empty input\",\"state\":\"OPEN\",\"labels\":[{\"name\":\"bug\"}],\"url\":\"https://github.com/acme/example/issues/%s\"}\\n' \"$3\" \"$3\"\n",
        )
        .unwrap();
    }

    /// Serves merged pull requests into main for `(branch, head)` pairs.
    fn merged_pull_requests(&self, branches: &[(&str, &str)]) {
        let prs = branches
            .iter()
            .enumerate()
            .map(|(number, (branch, head))| {
                serde_json::json!({
                    "number": number + 1, "state": "MERGED", "headRefName": branch,
                    "headRefOid": head, "baseRefName": "main", "isCrossRepository": false,
                })
            })
            .collect::<Vec<_>>();
        fs::write(self.bin.join("prs.json"), serde_json::to_vec(&prs).unwrap()).unwrap();
        fs::write(self.bin.join("gh"), "#!/bin/sh\ncat \"${0%/*}/prs.json\"\n").unwrap();
    }

    /// Serves every number as a pull request from `head`; other gh calls fail.
    fn fake_pull_request(&self, head: &str, cross_repository: bool) {
        fs::write(
            self.bin.join("gh"),
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  issue) printf '{{\"number\":%s,\"title\":\"Shared view\",\"state\":\"OPEN\",\"labels\":[],\"url\":\"https://github.com/acme/example/pull/%s\"}}\\n' \"$3\" \"$3\" ;;\n  pr) printf '%s\\n' '{{\"headRefName\":\"{head}\",\"isCrossRepository\":{cross_repository}}}' ;;\n  *) exit 99 ;;\nesac\n"
            ),
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
        "#!/bin/sh\nif [ \"$1\" = repo ]; then\n  printf '%s\\n' '{\"nameWithOwner\":\"acme/example\",\"defaultBranchRef\":{\"name\":\"main\"}}'\nelse\n  printf '{\"number\":%s,\"title\":\"Handle empty input\",\"state\":\"OPEN\",\"labels\":[{\"name\":\"bug\"}],\"url\":\"https://github.com/acme/example/issues/%s\"}\\n' \"$3\" \"$3\"\nfi\n",
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
