---
name: wt
description: Use wt to create, enter, list, and remove issue-linked Git worktrees, while routing standalone worktree requests to native Git. Trigger when the user asks for a worktree or starts work from a GitHub issue.
---

# wt worktrees

Keep issue-linked worktrees isolated without forcing unrelated work into the issue tracker.

## Choose the worktree path

- Existing GitHub issue: prefer `wt` when it is installed and the repository has `.wtconfig`.
- No existing GitHub issue: use native `git worktree` with a descriptive branch and path. A standalone worktree does not require a tracking issue.
- Already in a suitable linked worktree: reuse it.

Inspect `git status --short --branch` and `git worktree list --porcelain` before creating anything. Preserve unrelated worktree and branch changes.

## Issue-linked work

Run `wt add <issue-number-or-url>` from the repository that owns the issue. Use the returned path as the working directory for all subsequent work.

If `.wtconfig` is missing, explain that repository setup requires `wt init`; run it only when the user asks to configure the repository. If `wt` is unavailable, report that fact and use native Git when the user only needs isolation.

Use `wt list` to inspect managed worktrees and `wt remove <issue-number>` to remove one. Treat removal as destructive: inspect its status first and never add `--force` unless the user explicitly authorizes discarding changes.

## Standalone work

Resolve a descriptive branch, an explicit base ref, and a non-conflicting path before running `git worktree add`. Do not create a GitHub issue merely to satisfy `wt`'s issue argument.
