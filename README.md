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

## Safety and automation

Without `--force`, `wt remove` refuses to delete a worktree with tracked
changes, unknown files, modified copied files, or files outside `disposable`
paths. Even forced removal keeps the Git branch.

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
