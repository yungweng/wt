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
Authenticate the GitHub CLI once:

```sh
gh auth login
cargo install --git https://github.com/yungweng/wt
```

## Quick start

Run the setup wizard in a GitHub repository:

```sh
wt init
```

The default root is `~/Developer/worktrees`. The wizard stores another choice
in `$XDG_CONFIG_HOME/wt/config` or `~/.config/wt/config`. For scripted setup:

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
    port = WEB_PORT
    port = DATABASE_PORT

    bootstrap = make setup
    teardown = docker compose down --remove-orphans

    disposable = .cache
    disposable = web/node_modules
```

Commit `.wtconfig`, but do not commit the secret files it names.

### Files

`env` selects the primary env file. `copy` adds more files. Paths must be
relative, must stay inside the repository, and must point to regular files.
Symlinks are rejected.

The wizard suggests ignored `.env` files. `wt` never copies an unlisted file.

### Ports and Compose

Each `port` names a numeric variable in the primary env file. `wt` starts at
that value, finds a free host port, and stores a lease under
`$XDG_STATE_HOME/wt` or `~/.local/state/wt`.

References using `localhost:<port>` or `127.0.0.1:<port>` are updated in every
copied env file. Other hosts and container ports remain unchanged.

`compose = true` writes a unique `COMPOSE_PROJECT_NAME` to the primary env
file.

### Commands and trust

`bootstrap` runs after setup. `teardown` runs before removal. Both use
`/bin/sh` in the worktree.

`wt init` shows and trusts the exact configuration it writes. If `.wtconfig`
changes, review it and run:

```sh
wt trust
```

For automation, use `wt trust --yes` only after another step has reviewed the
file. Skip setup or teardown explicitly when needed:

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

## License

MIT
