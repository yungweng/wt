use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    io::{self, Write},
    path::Path,
    process::Command,
};

use clap_complete::{engine::CompletionCandidate, env::EnvCompleter};

use crate::{app, state::Store};

pub struct Fish;

impl EnvCompleter for Fish {
    fn name(&self) -> &'static str {
        "fish"
    }

    fn is(&self, name: &str) -> bool {
        name == self.name()
    }

    fn write_registration(
        &self,
        var: &str,
        name: &str,
        bin: &str,
        completer: &str,
        buf: &mut dyn Write,
    ) -> io::Result<()> {
        let mut script = Vec::new();
        clap_complete::env::Fish.write_registration(var, name, bin, completer, &mut script)?;
        // clap_complete 4.6.9 passes the raw Fish token, including quotes and escapes.
        // Tokenize it without evaluating shell code; printf preserves an empty token.
        // Remove this adapter once upstream tokenizes Fish input.
        let script = String::from_utf8_lossy(&script).replace(
            "(commandline --current-token)",
            r"(printf '%s\\n' (commandline --current-token --tokenize))",
        );
        buf.write_all(script.as_bytes())
    }

    fn write_complete(
        &self,
        cmd: &mut clap::Command,
        args: Vec<OsString>,
        current_dir: Option<&Path>,
        buf: &mut dyn Write,
    ) -> io::Result<()> {
        clap_complete::env::Fish.write_complete(cmd, args, current_dir, buf)
    }
}

pub fn branches(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(output) = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)%09%(symref)",
            "refs/heads/",
            "refs/remotes/origin/",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let output = String::from_utf8_lossy(&output.stdout);
    candidates(
        current,
        output.lines().filter_map(|line| {
            let (name, symbolic) = line.split_once('\t')?;
            if !symbolic.is_empty() {
                return None;
            }
            name.strip_prefix("refs/heads/")
                .or_else(|| name.strip_prefix("refs/remotes/origin/"))
                .map(str::to_owned)
        }),
    )
}

pub fn worktrees(current: &OsStr) -> Vec<CompletionCandidate> {
    let records = (|| {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        anyhow::ensure!(output.status.success(), "not in a repository");
        let directory = String::from_utf8(output.stdout)?;
        let repository = app::repository_slug(Path::new(directory.trim()))?;
        let records = Store::open_readonly()?.records()?;
        Ok::<_, anyhow::Error>((repository, records))
    })();
    let Ok((repository, records)) = records else {
        return Vec::new();
    };
    candidates(
        current,
        records
            .into_iter()
            .filter(|record| record.repository.eq_ignore_ascii_case(&repository))
            .map(|record| {
                record
                    .issue
                    .map_or(record.branch, |issue| issue.to_string())
            }),
    )
}

fn candidates(current: &OsStr, values: impl Iterator<Item = String>) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    values
        .filter(|value| value.starts_with(prefix))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}
