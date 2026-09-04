# wt

`wt` creates an isolated Git worktree from a GitHub issue and prepares it for
development.

```console
$ wt add 42
/Users/alex/Developer/worktrees/example-api/fix-42-handle-empty-input
```

It copies only approved files, assigns free ports, isolates Docker Compose,
and can run a trusted setup command. There is no dashboard, daemon, or plugin
system.

## Install

You need Git, the [GitHub CLI](https://cli.github.com/), and Rust 1.85 or newer.
Process-port isolation also needs [direnv](https://direnv.net/).
Authenticate the GitHub CLI once:

```sh
gh auth login
cargo install --git https://github.com/yungweng/wt
```

### Agent skill

Install the companion `wt` skill globally for Claude Code and Codex:

```sh
npx --yes skills add yungweng/wt --skill wt --global \
  --agent claude-code --agent codex --yes
```

The skill teaches agents to use `wt` for work tied to an existing GitHub issue
and native Git for standalone worktrees. It never requires creating an issue
just to create a worktree.

## Quick start

Run the setup wizard in a GitHub repository:

```sh
wt init
```

The wizard detects the base branch, ignored environment files, development
servers, published Docker Compose ports, package-manager setup commands, and
generated directories. It reads tracked `package.json` scripts for Next.js,
Vite, and Wrangler. It shows whether each server uses a leased port, an
automatic fallback, or a fixed port.

If a README tells you to copy a root `.env.example`, the wizard offers to make
the target file. It never treats an example file as a secret. Compose files
outside the repository root appear as information and do not enable Compose
isolation.

`wt` warns when a development port can collide, runtime files hard-code the
original localhost port, or Devbox stores a Go cache inside each worktree. A
warning changes the default answer to **No**.

The default root is `~/Developer/worktrees`. `wt` stores another choice in
`$XDG_CONFIG_HOME/wt/config` or `~/.config/wt/config`. For scripted setup:

```sh
wt init --root /worktrees --base main --yes
```

Create a worktree from an issue number or URL:

```sh
wt add 42
wt add https://github.com/acme/example-api/issues/42
```

Open it directly:

```sh
cd "$(wt add 42)"
```

`wt add` writes only the path to stdout. Interactive progress and errors go to
stderr, so command substitution remains safe.

List or remove managed worktrees:

```sh
wt list
wt remove 42
```

Removal keeps the Git branch.

## Repository configuration

`wt init` writes `.wtconfig` using Git's config format:

```ini
[wt]
    base = main
    env = .env
    copy = web/.env.local
    compose = true

    port = API_PORT
    port = DATABASE_PORT
    port = PORT:3000

    bootstrap = make setup
    teardown = docker compose down --remove-orphans

    disposable = .cache
    disposable = web/node_modules
```

Commit `.wtconfig` and any `.envrc` or `.gitignore` changes made by `wt init`
on the configured base branch. `wt` starts new worktrees from that local branch
when it exists. Do not commit the secret files named by `.wtconfig`.

### Files

`env` selects the primary env file. `copy` adds more files. Paths must be
relative, must stay inside the repository, and must point to regular files.
Symlinks are rejected.

The wizard suggests ignored `.env*` and `.dev.vars` files. It excludes
templates, backups, dependency caches, and build output. `wt` never copies an
unlisted file.

### Ports and Compose

Use `port = KEY` when `KEY` contains a numeric port in the primary env file.
`wt` rewrites that copied file.

Use `port = KEY:DEFAULT` when a server reads the port from its process
environment. For example, Next.js needs `port = PORT:3000` because it reads
`PORT` before loading `.env` files. `wt` writes the leased value to `.wt.env`.
During setup, it adds these lines to the repository when needed:

```sh
# .envrc
dotenv_if_exists .wt.env

# .gitignore
/.wt.env
```

Commit both changes on the base branch before creating a worktree. `wt` starts
at the configured value, finds a free host port, and stores a lease under
`$XDG_STATE_HOME/wt` or `~/.local/state/wt`.

References using `localhost:<port>` or `127.0.0.1:<port>` are updated in every
copied env file. Other hosts and container ports remain unchanged.

`compose = true` writes a unique `COMPOSE_PROJECT_NAME` to the primary env
file.

### Commands and trust

`bootstrap` runs after setup. `teardown` runs before removal. Both use
`/bin/sh` in the worktree.

`wt init` trusts the commands it writes. If those commands change, an
interactive `wt add` shows them and asks whether to allow and remember them.
Changes to other settings do not revoke command trust.

In non-interactive automation, review the file before running the hidden
`wt trust --yes` command. Skip setup or teardown explicitly when needed:

```sh
wt add 42 --no-bootstrap
wt remove 42 --skip-teardown
```

## Removal safety

Without `--force`, `wt remove` refuses to delete:

- tracked changes;
- unknown untracked or ignored files;
- copied files changed since creation;
- files outside configured `disposable` paths.

`disposable` is an explicit allowlist for generated data. A forced removal
still keeps the branch:

```sh
wt remove 42 --force
```

## Automation

Use stable tab-separated output:

```sh
wt list --porcelain
wt list --all --porcelain
```

Override storage paths when running in an isolated environment:

```sh
WT_WORKTREE_ROOT=/tmp/worktrees \
WT_STATE_HOME=/tmp/wt-state \
wt add 42
```

## Branch names

`wt` combines the issue number and title. Common labels select a prefix:

| Labels | Prefix |
| --- | --- |
| `bug`, `type: bug`, `type/bug` | `fix/` |
| `feature`, `enhancement`, `type: feature` | `feat/` |
| `documentation`, `docs` | `docs/` |
| anything else | `work/` |

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Integration tests use temporary local Git repositories and a fake `gh`
executable. They do not need network access or a GitHub account.

## Limits

- GitHub issues only.
- macOS and Linux only.
- Port leases coordinate `wt` processes. Another program can still claim a
  checked port before the development service starts.
- Script detection recognizes common Next.js, Vite, and Wrangler commands. It
  does not parse arbitrary shell programs or every framework configuration.

## License

MIT
