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
| `wt add 57` | Check out pull request 57 in a worktree |
| `wt add feat/my-change` | Create a worktree for a branch |
| `wt list` | List worktrees created by `wt` |
| `wt remove 42` | Safely remove by issue or branch |
| `wt clean` | Preview and confirm removal of safely merged worktrees |
| `wt clean --force` | Also remove merged worktrees with local changes |
| `wt doctor` | Check a worktree for missing private files and shadowed ports |
| `wt shell fish` | Print a shell function that changes into new worktrees |

## Install

You need Git and the [GitHub CLI](https://cli.github.com/).
[direnv](https://direnv.net/) is only required for process-port isolation.

```sh
gh auth login
brew install yungweng/tap/wt
```

Homebrew also installs tab completion for Bash, Zsh, and Fish. Without
Homebrew, install with Cargo (Rust 1.85 or newer):

```sh
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
Human-readable output groups worktrees by repository in compact tables.
Columns align by terminal display width, including Unicode branch names.
Long cells are shortened with `…` to fit the terminal. Paths use `~` for your
home directory, or `…/directory` when the parent path does not fit. Use
`--porcelain` for full branch names and paths without truncation.
In terminals supporting OSC 8 hyperlinks, issue numbers in `wt list` and
`wt clean` link to their GitHub issues. Links are built locally, without
network requests. Redirected output and `--porcelain` remain plain text.

## Change into new worktrees

A program cannot change its shell's directory, so `wt shell` prints a small
`wt` function that runs `wt add` and then changes into the printed path.
Add one line to your shell's startup file and open a new shell:

| Shell | File | Line |
| --- | --- | --- |
| Fish | `~/.config/fish/config.fish` | `wt shell fish \| source` |
| Zsh | `~/.zshrc` | `eval "$(wt shell zsh)"` |
| Bash | `~/.bashrc` | `eval "$(wt shell bash)"` |

`wt add 42` then creates the worktree and changes into it; for an existing
worktree it only changes into it. Other subcommands, failures, and `--help`
pass through unchanged. When stdout is not a terminal, for example in
`cd "$(wt add 42)"` or a pipe, the function prints the path as before, so
scripts behave the same with or without it. The line generates the function
from the installed binary, so it stays current after upgrades. Bash 3.2, the
version macOS ships, is supported; use `eval` there, not `source <(...)`.

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
# or: wt add https://github.com/acme/example-api/pull/57
# or: wt add feat/my-change
```

Numbers and GitHub issue or pull request URLs select issues or pull requests.
Every other valid Git branch name creates a standalone worktree, so
`wt add test` needs no issue.

A pull request opens on its head branch, freshly fetched from `origin` and
tracking it, so you can review, commit, and push. Pull requests from forks are
refused; use `gh pr checkout` for those.

`wt add` prints only the new path to stdout, so command substitution is safe.
Progress, errors, and the issue or pull request URL go to stderr. The URL
appears after setup, and also when the worktree already exists. Common issue labels produce `fix/`, `feat/`,
or `docs/` branches; other issues use `work/`. A branch name skips issue lookup
and starts at `wt.base`, or at the current branch when no base is configured.
`wt` fetches the base from `origin` first, so new worktrees always start at
the latest remote commit; a local base branch is used only when the base is
missing on `origin`. If the branch already exists locally or on `origin`,
`wt` checks it out.

## Configure a repository

`wt init` writes `.wtconfig` in Git's config format:

```ini
[wt]
    base = main
    env = .env
    copy = web/.env.local
    copy = config/certs
    copy = **/*.pem
    compose = true

    port = API_PORT
    port = PORT:3000

    bootstrap = make setup
    teardown = docker compose down --remove-orphans

    disposable = .cache
    disposable = web/node_modules
```

- `env` and `copy` select untracked paths to copy: a file, a directory (every
  file below it), or a glob such as `**/*.pem` (its untracked matches). Paths
  must stay inside the repository. Ignored files named `*.local` or
  `*.local.*`, such as `CLAUDE.local.md` or `.claude/settings.local.json`, are
  copied without configuration. A symlink that points outside the repository,
  for example notes linked from a dotfiles directory, is recreated as the same
  symlink so both checkouts share it; other symlinks are copied as regular
  files. Files inside wholly ignored directories and paths that already exist
  in the new worktree are skipped. A configured path that is missing from the
  checkout is looked up again after `bootstrap`: files the setup command
  generates get the same port rewrite and are managed like copied files. A
  path that still does not exist only produces a warning.
- `port = KEY` rewrites a port stored in the primary env file, together with
  `localhost:<port>` and `127.0.0.1:<port>` references in every copied file.
  `KEY:DEFAULT` leases a process port through `.wt.env`; this form requires
  direnv. When `.envrc` loads `.wt.env`, the file lists every leased port and
  the Compose project name, so values from user-wide env files loaded earlier
  in `.envrc` cannot shadow the worktree's ports.
- `compose = true` gives each worktree a unique `COMPOSE_PROJECT_NAME`.
- `bootstrap` runs after creation; `teardown` runs before removal. `wt` asks
  again if either trusted command changes.
- `disposable` lists generated paths that `wt remove` may discard.

The default worktree root is `~/Developer/worktrees`. Change it with
`wt init --root /absolute/path` or `WT_WORKTREE_ROOT`.

If the repository uses direnv and its `.envrc` is allowed in the checkout,
`wt add` runs `direnv allow` in the new worktree. The worktree's `.envrc` is
byte-identical to the checkout's, so nothing new is trusted; a different
`.envrc` still needs a manual `direnv allow`.

## Check a worktree

```sh
wt doctor
```

Run it inside a worktree created by `wt`. It compares the worktree with the
main checkout and reports:

- Ignored files of the main checkout that the worktree lacks, such as
  generated certificates or a local Compose file, minus `disposable` paths
  and common caches. Add them as `copy` entries or copy them by hand.
- Managed files that were deleted from the worktree.
- An `.envrc` that direnv has not allowed, so `.wt.env` is not loaded.
- Leased ports that the shell or `.envrc` overrides with another value. Docker
  Compose prefers the environment over `.env`, so a user-wide env file with
  `API_PORT=8080` silently sends every worktree to the same port.

`wt doctor` exits unsuccessfully when it finds a problem.

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
cleanup, and Git removal. Generated files (`disposable` paths) are moved into a
`.wt-trash` directory next to the worktree, which takes milliseconds even for
large `node_modules` trees. A detached background process then deletes the
trash with at most four workers; interrupted deletions are retried by the next
removal. Paths on another filesystem are deleted in place. Symlinks are
unlinked, not followed. Cleanup failures report the affected path and keep
the state record. Forced removal also moves generated files aside and lets Git
delete everything else.

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
wt clean --force    # Also remove merged worktrees with local changes
```

Cleanup covers `wt`-managed worktrees in the current repository and clone.
It checks whether each worktree's current commit is an ancestor of `wt.base`
(or the GitHub default branch when no base is configured). It also recognizes
squash and rebase merges through a merged GitHub PR whose final head contains
the worktree's current commit. This includes worktrees left behind when more
commits were added to the PR before merging. Newer or divergent local commits
keep the worktree out of cleanup, with an explicit explanation that the PR
is merged but does not include those commits. PRs from forks do not qualify
through this fallback.

The base must resolve locally. Cleanup does not fetch or update branches;
update your base first to include recent merges. PR checks use the
[GitHub CLI](https://cli.github.com/manual/gh_pr_list); unavailable or
inconclusive checks leave the worktree in place. Read-only checks use at most
four workers and share one lookup of the 100 most recent merged PRs. When
that page is full and no qualifying PR was found, cleanup checks up to 100
merged PRs for the individual branch. Commit ancestry is checked locally, or
through GitHub's comparison API when the final PR commit is unavailable locally.
Branches with no commits ahead of the base also qualify,
even if no PR was created. Age alone never qualifies a worktree.

Cleanup skips locked worktrees, the current worktree, the base branch, detached or switched
branches, other clones, and unmerged branches. The preview groups worktrees
under “Ready to remove”, “Needs --force”, and “Skipped”, with one table row
per worktree.

“Needs --force” lists merged worktrees that fail the same file safety checks
as `wt remove` (changed copied files, tracked changes, or unmanaged files).
`wt clean --force` moves them to “Ready to remove” and shows what each one
loses. Records whose worktree directory is already gone have nothing left to
lose and are always ready. The merge check runs first,
so unmerged work is never offered, even with `--force`. Local changes also
require a merged PR for the branch: a new branch without commits is an
ancestor of the base, too, and may hold work in progress. If a forced candidate
gains new changes after the preview, for example from its teardown, it is kept.
Missing worktrees only lose their `wt` record and Git's stale worktree entry
(`git worktree prune`). Repeated worktree paths are omitted; use `wt list` to
see them. `--dry-run` never runs teardown or removes files. Non-interactive
removal requires `--yes`, also together with `--force`.

Removal rechecks candidates after confirmation, runs trusted teardown commands,
and keeps all branches. Up to four worktrees are removed in parallel behind a
single status line; `--verbose` removes them one at a time with raw logs. Use `--skip-teardown` to leave services running.
If a candidate changes or removal fails, cleanup reports it, continues with
the others, and exits unsuccessfully. Already removed worktrees stay removed.

## Safety and automation

Without `--force`, `wt remove` refuses to delete a worktree with tracked
changes, unknown files, modified copied files, or files outside `disposable`
paths. Even forced removal keeps the Git branch. If the worktree directory
was already deleted by hand, `wt remove` drops the record and Git's stale
entry without teardown.

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
