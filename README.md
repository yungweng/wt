# wt

Turn a GitHub issue or branch into an isolated, ready-to-code Git worktree.

```console
$ wt add 42
/Users/alex/Developer/worktrees/example-api/fix-42-handle-empty-input
```

`wt` creates the branch and can prepare everything the new worktree needs:
approved files, free development ports, an isolated Docker Compose project,
and a trusted setup command. When the work is done, it removes the worktree
without deleting the branch or silently throwing away changes.

| Command | What it does |
| --- | --- |
| `wt init` | Configure the current repository |
| `wt add 42` | Create a worktree for issue 42 |
| `wt add feat/my-change` | Create a worktree for a branch |
| `wt list` | List worktrees created by `wt` |
| `wt remove 42` | Safely remove by issue or branch |
| `wt clean` | Preview and confirm removal of safely merged worktrees |

## Install

You need Git, the [GitHub CLI](https://cli.github.com/), and Rust 1.85 or newer.
[direnv](https://direnv.net/) is only required for process-port isolation.

```sh
gh auth login
cargo install --git https://github.com/yungweng/wt
```

From a local checkout:

```sh
make          # Build target/release/wt
make check    # Format, lint, and test
make install  # Replace the installed wt binary
```

## Tab completion

Enable completion in your shell after installing `wt`:

**Zsh:** add this to `~/.zshrc`, after your existing `compinit` setup:

```zsh
# If completion is not initialized yet: autoload -Uz compinit; compinit
source <(COMPLETE=zsh wt)
```

**Bash:** add this to `~/.bashrc`:

```bash
source <(COMPLETE=bash wt)
```

**Fish:** put this in `~/.config/fish/completions/wt.fish` (create the
directory if needed):

```fish
COMPLETE=fish wt | source
```

Open a new shell to activate completion. These lines generate registration
from the installed binary, keeping it in sync after upgrades; do not replace
them with a saved copy of the generated script.

Tab suggests commands, flags, and paths. `wt add` and `wt init --base` also
suggest local branches and cached `origin` branches without fetching.
`wt remove` suggests managed issue numbers and standalone branch names for
the current repository. Completion never contacts GitHub or runs setup or
teardown commands, and it does not create or update state files.

`wt list` shows each worktree's current branch, or labels it as detached,
missing, or unavailable. If it differs from the recorded branch, a `managed:`
annotation shows the original reference. Removal and completion still use the
managed issue number or original standalone branch name. `wt list --porcelain`
keeps its stable three-column format with the recorded branch.

## Start a worktree

Run the setup wizard once inside a GitHub repository:

```sh
wt init
```

The wizard finds the base branch, ignored environment files, development
servers, Docker Compose ports, setup commands, and generated directories. It
writes the choices to `.wtconfig`. Commit that file and any `.envrc` or
`.gitignore` changes before creating a worktree.

Then start work from an issue number, URL, or branch name:

```sh
cd "$(wt add 42)"
# or: wt add https://github.com/acme/example-api/issues/42
# or: wt add feat/my-change
```

Numbers and GitHub issue URLs select issues. Every other valid Git branch name
creates a standalone worktree, so `wt add test` needs no issue.

`wt add` prints only the new path to stdout, so command substitution is safe.
Progress and errors go to stderr. Common issue labels produce `fix/`, `feat/`,
or `docs/` branches; other issues use `work/`. A branch name skips issue lookup
and starts at `wt.base`, or at the current branch when no base is configured.
If the branch already exists locally or on `origin`, `wt` checks it out.

## Configure a repository

`wt init` writes `.wtconfig` in Git's config format:

```ini
[wt]
    base = main
    env = .env
    copy = web/.env.local
    compose = true

    port = API_PORT
    port = PORT:3000

    bootstrap = make setup
    teardown = docker compose down --remove-orphans

    disposable = .cache
    disposable = web/node_modules
```

- `env` and `copy` select untracked files to copy. Paths must stay inside the
  repository, point to regular files, and cannot be symlinks.
- `port = KEY` rewrites a port stored in the primary env file. `KEY:DEFAULT`
  leases a process port through `.wt.env`; this form requires direnv.
- `compose = true` gives each worktree a unique `COMPOSE_PROJECT_NAME`.
- `bootstrap` runs after creation; `teardown` runs before removal. `wt` asks
  again if either trusted command changes.
- `disposable` lists generated paths that `wt remove` may discard.

The default worktree root is `~/Developer/worktrees`. Change it with
`wt init --root /absolute/path` or `WT_WORKTREE_ROOT`.

## Progress and setup time

`wt add` shows a compact repository header, completed steps, and a live elapsed
timer for the current step. Routine bootstrap and teardown output stays hidden
on a terminal; failures print the captured diagnostics. Use `--verbose` (`-v`)
for live raw logs or commands that prompt for input. Redirected runs continue
to stream logs to stderr, and stdout contains only the destination path.

Set `NO_COLOR=1` to disable color. `TERM=dumb` also disables animation.
A connected left rail and rotating indicator show progress; completed steps use
diamonds. Labels and timings stay aligned. Setup and trust prompts share this layout.
The status line uses no full-width padding, and the destination appears once.

Bootstrap runs without holding the global state lock, so unrelated worktrees
can be added while it runs. Adds and removals for the same worktree still wait
for its bootstrap to finish.

Normal removal shows separate timings for safety checks, teardown, generated-file
cleanup, and Git removal. Generated-file cleanup uses at most four workers and
waits for them to finish before removing the worktree. Symlinks are unlinked,
not followed. Cleanup failures report the affected path and keep the state record.
Forced removal continues to delegate directly to Git.

Bootstrap time includes the repository's configured setup command. A fresh
frontend dependency installation can take much longer than Git checkout.
To create the worktree without installing dependencies:

```sh
wt add 42 --no-bootstrap
```

Run the configured bootstrap command from that worktree when you need its
build tools and dependencies. Skipping bootstrap does not make them ready.

## Clean up merged worktrees

```sh
wt clean --dry-run  # Preview candidates and reasons for skipped worktrees
wt clean            # Preview, then ask before removal (default: No)
wt clean --yes      # Remove eligible candidates without prompting
```

Cleanup covers `wt`-managed worktrees in the current repository and clone.
It checks whether each worktree's current commit is an ancestor of `wt.base`
(or the GitHub default branch when no base is configured). It also recognizes
squash and rebase merges through a merged GitHub PR whose head commit exactly
matches the worktree. New commits after a PR merge keep the worktree out of
cleanup. PRs from forks do not qualify through this fallback.

The base must resolve locally. Cleanup does not fetch or update branches;
update your base first to include recent merges. PR checks use the
[GitHub CLI](https://cli.github.com/manual/gh_pr_list); unavailable or
inconclusive checks leave the worktree in place. At most 100 merged PRs per
branch are checked. Branches with no commits ahead of the base also qualify,
even if no PR was created. Age alone never qualifies a worktree.

Cleanup skips locked worktrees, the current worktree, the base branch, detached or switched
branches, missing paths, other clones, and worktrees that fail the same file
safety checks as `wt remove`. The preview shows each candidate and skip reason.
`--dry-run` never runs teardown or removes files. Non-interactive removal
requires `--yes`; there is no `--force` option.

Removal rechecks candidates after confirmation, runs trusted teardown commands,
and keeps all branches. Use `--skip-teardown` to leave services running.
If a candidate changes or removal fails, cleanup reports it, continues with
the others, and exits unsuccessfully. Already removed worktrees stay removed.

## Safety and automation

Without `--force`, `wt remove` refuses to delete a worktree with tracked
changes, unknown files, modified copied files, or files outside `disposable`
paths. Even forced removal keeps the Git branch.

Ignored directories containing only empty directories do not block removal.
Other unmanaged ignored paths still block it, even when plain `git status`
reports a clean worktree. Symlinks are not followed when checking for empty
directory trees.

```sh
wt init --root /worktrees --base main --yes
wt add 42 --no-bootstrap
wt remove 42 --skip-teardown
wt list --porcelain
wt list --all --porcelain
```

`--porcelain` produces stable tab-separated output. Set `WT_STATE_HOME` to
override state storage in isolated automation. If a setup or teardown command
changes, review `.wtconfig` before running the hidden `wt trust --yes` command
in a non-interactive environment.

## Agent skill

Install the companion skill to teach Claude Code and Codex when to use `wt`:

```sh
npx --yes skills add yungweng/wt --skill wt --global \
  --agent claude-code --agent codex --yes
```

The skill uses `wt` for issue-linked and standalone worktrees. It never creates
an issue just to create a worktree.

## Limits

- Issue lookup supports GitHub.com only; branch creation needs no issue.
- macOS and Linux only.
- Port leases reduce collisions between `wt` worktrees, but another program
  can still claim a checked port before the development server starts.
- Automatic server detection covers common Next.js, Vite, and Wrangler
  commands, not arbitrary shell scripts or every framework configuration.

## Development

```sh
make check
```

Integration tests use local repositories and a fake `gh`; they need no network
access or GitHub account.

## License

MIT
