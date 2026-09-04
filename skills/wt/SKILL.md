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
- Standalone work: run `wt add <branch>` with an exact, descriptive branch name.

Use the path printed by `wt add` as the working directory. A number or GitHub
issue URL selects an issue; every other valid Git branch name creates a
standalone worktree without a tracking issue.

`.wtconfig` is optional. Run `wt init` only when the user asks to configure
copied files, ports, Docker Compose, bootstrap, or teardown. If `wt` is not
installed, report that blocker.

## List and remove

Use `wt list` to inspect managed worktrees. Remove one with
`wt remove <issue-number-or-branch>`. Inspect its status first; use `--force`
only when the user explicitly authorizes discarding changes.
