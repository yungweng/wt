---
name: wt
description: Use wt to create, locate, list, and remove managed Git worktrees for GitHub issues and standalone branches. Trigger when the user asks for a worktree or starts work from an issue.
---

# wt worktrees

Use `wt` for issue-linked and standalone worktrees.

## Before creating

Run `git status --short --branch` and `git worktree list --porcelain`. Reuse an
existing suitable worktree and preserve unrelated changes.

## Create

- Existing issue: run `wt add <issue-number-or-url>` from its repository.
- Existing pull request: run `wt add <pr-number-or-url>` to check out its head
  branch. Fork pull requests are refused; use `gh pr checkout` instead.
- Standalone work: run `wt add <branch>` with an exact, descriptive branch name.

Use the path printed by `wt add` on stdout as the working directory; stderr
also shows the issue or pull request URL. A number or GitHub
issue or pull request URL selects an issue or pull request; every other valid
Git branch name creates a standalone worktree without a tracking issue.

`.wtconfig` is optional. Run `wt init` only when the user asks to configure
copied files, ports, Docker Compose, bootstrap, or teardown. If `wt` is not
installed, report that blocker.

## Check

Run `wt doctor` inside a worktree when services fail to start, ports collide,
or local files seem missing. It lists ignored files of the main checkout that
the worktree lacks, a blocked `.envrc`, and leased ports that the shell or
`.envrc` shadows. Fix what it reports; add recurring files as `copy` entries
in `.wtconfig`.

## List and remove

Use `wt list` to inspect managed worktrees. Remove one with
`wt remove <issue-number-or-branch>`. Inspect its status first; use `--force`
only when the user explicitly authorizes discarding changes. The same applies
to `wt clean --force`, which also removes merged worktrees with local changes.
