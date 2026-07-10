# tinycd

`tinycd` runs a small deployment pipeline when a Git remote changes, when it
receives a webhook, or both:

```text
sync -> install -> start
```

Each deployment gets a new folder. Older deployments are retained for rollback,
and `current` is atomically moved to the newest launched release.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/maxz411/tinycd/main/install.sh | sh
```

On Windows:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/maxz411/tinycd/main/install.ps1 | iex"
```

The script downloads the latest [release](https://github.com/maxz411/tinycd/releases),
verifies its checksum, and installs a single binary to `~/.local/bin`
(`%LOCALAPPDATA%\tinycd\bin` on Windows). The Linux binaries are fully static,
so they run on any distribution. `git` must be on `PATH` at runtime. To build
from source instead, run `cargo install --path .` from a checkout.

## Quick start

Commit a `.tinycd/config.toml` to the project being deployed:

```toml
install = "npm ci"
start = "npm start"
```

Then point tinycd at the project's Git URL and a directory to deploy into:

```sh
tinycd git@github.com:example/app.git ~/apps/example
```

tinycd clones the repository, reads `.tinycd/config.toml` from the clone, installs
and starts it, and redeploys whenever the remote changes. The directory
defaults to the current one, and a small `.tinycd/config.toml` recording the URL is
written into it, so `tinycd ~/apps/example` (or a plain `tinycd` inside it)
restarts the same deployment after a reboot.

Alternatively, run tinycd from inside a checkout, or point it at one:

```sh
tinycd            # track this checkout's origin
tinycd ~/code/app # track that checkout's origin
```

Deployments then live under `~/.tinycd/<name>-<hash>` instead of the working
tree, so the checkout stays an ordinary place to work: commit and push from
it, or push from another machine, and tinycd deploys the new commit either
way. If the checkout has no config yet and the terminal is interactive, a
setup wizard walks through creating `.tinycd/config.toml` — detected remote,
suggested install and start commands, and where releases should live.

With no other configuration, tinycd polls the remote every 30 seconds,
shallow-clones new releases, keeps five releases, and uses `<root>/interlock`
as the deployment interlock. A fresh setup deploys immediately. After a
restart, tinycd starts the existing `current` release, then deploys anything
that was pushed while it was down.

The defaults are:

```toml
poll = 30
interlock = "<root>/interlock"
shell = ["sh", "-c"]                          # ["cmd", "/C"] on Windows
sync = 'git clone --depth 1 "$TINYCD_REPO" .' # "%TINYCD_REPO%" on Windows
keep = 5
```

`root` — the folder holding the deployments and the `current` link — defaults
to `<dir>/.tinycd` when tinycd is given a Git URL, and to
`~/.tinycd/<name>-<hash>` when it tracks a checkout, so `.tinycd/` is
tinycd's one footprint either way. `repo` defaults to the tracked checkout's
`origin`, and `ref` — a branch or tag to deploy instead of the remote HEAD —
is unset, as are `hook`, `install`, `share`, and `check`. `start` has no
default because guessing how to launch an arbitrary project would be unsafe.

On an interactive terminal the daemon shows a live dashboard — tracked
repository, retained releases with the current one marked, and the activity
feed — and quits gracefully on `q`. Pass `--no-tui` for plain line output;
non-interactive runs (systemd, pipes, CI) are always plain.

Useful one-off invocations:

```sh
tinycd --dry-run        # build and start one throwaway release, then stop it
tinycd --status         # deployed release, commit, log path, daemon state
tinycd --rollback       # repoint current at the previous release (daemon stopped)
tinycd --rollback <id>  # or at a specific retained release
```

## Configuration

tinycd reads up to two config files: the local `.tinycd/config.toml` in the tracked
directory (or the file named by `--config`), and the `.tinycd/config.toml` inside the
repository, re-read from every synced release. The repository's file provides
`shell`, `env`, `install`, and `start`, so a project carries its own
deployment recipe and changes take effect on the next deployment. Everything
else — what to watch and where to keep releases — only comes from the machine
running tinycd.

All runtime settings can be placed in the local `.tinycd/config.toml`:

```toml
repo = "git@github.com:example/app.git"
ref = "production"
poll = 30
hook = "127.0.0.1:8080"
root = "."
interlock = "interlock"
share = ["data/", ".env"]
shell = ["sh", "-c"]
sync = 'git clone --depth 1 --branch "$TINYCD_REF" "$TINYCD_REPO" .'
install = "npm ci"
start = "npm start"
check = "curl -sf localhost:3000/health"
keep = 5

[env] # keep this table last: keys below a table header belong to it
PYTHONPATH = "/home/me/code"
```

See [config.example.toml](config.example.toml) for a copyable file. CLI values
and `TINYCD_TOKEN` override the local file, the local file overrides the
repository's file, and both override built-in defaults; `--env KEY=VALUE`
entries override matching `[env]` keys. Relative paths in a local file are
resolved relative to the file's directory — `.tinycd/` — so `root = "."`
keeps releases inside `.tinycd/` itself.

The default files are optional. Passing `--config <path>` makes that specific
file required. Unknown settings and invalid values are rejected, including a
tinycd setting accidentally placed under `[env]` — everything below a TOML
table header belongs to that table, which is also why `[env]` goes last (or
use dotted keys like `env.PYTHONPATH = "..."` anywhere in the file).

`share` names paths that stay out of Git but must survive from release to
release — vendored libraries, `.env` files, SQLite databases, upload dirs.
Each entry is linked into the release from `<root>/shared/` after sync, so
every release sees the same files. A trailing slash marks an entry that
should be created as a directory when `<root>/shared/` does not have it yet.

`check` gates deployments: a started release must stay up for three seconds,
and pass the check command (retried for up to 30 seconds) when one is set,
before `current` moves. A release that fails is torn down and the previous
release is restarted, so a bad push degrades to a logged failure instead of
an outage. `tinycd --dry-run` runs the same pipeline and verification against
a throwaway folder and exits, without touching the deployed state.

The sync, install, and start commands run through `shell` and inherit tinycd's
environment plus the `[env]` table. tinycd does not manage language runtimes
or virtual environments; compose the commands instead. One environment shared
by every release is just absolute paths:

```toml
install = "/home/me/app-venv/bin/pip install -r requirements.txt"
start = "/home/me/app-venv/bin/python -m uvicorn app:app"

[env]
PYTHONPATH = "/home/me/code"
```

## Webhooks

Setting only `hook` enables hook-only mode. Setting both `hook` and `poll`
enables both modes; polling skips commits a webhook already deployed. A
webhook always deploys, even when the remote has not changed, so it doubles
as a redeploy button.

Webhook mode requires a token containing at least 32 printable non-space
ASCII bytes. Prefer the environment so the token is not stored in the config
or placed in process arguments:

```sh
export TINYCD_TOKEN="$(openssl rand -hex 32)"
tinycd

curl -X POST \
  -H "Authorization: Bearer $TINYCD_TOKEN" \
  http://localhost:8080/deploy
```

The forges' native webhooks are accepted directly — no proxy shim needed.
Use the token as the GitLab webhook's secret token (sent as
`X-Gitlab-Token`) or as the GitHub webhook's secret (verified as the
`X-Hub-Signature-256` HMAC of the request body).

A token may be placed in `.tinycd/config.toml`, but on Unix the file must not be
readable by group or other users. Use HTTPS through a reverse proxy whenever
the webhook crosses an untrusted network. Tokens and repository URLs supplied
to the sync command are removed from install and start environments.

## Deployments

The root — printed at startup — is laid out as:

```text
<root>/
├── current -> deployments/1751392800000
├── head
├── lock
├── logs/
│   └── 1751392800000.log
├── shared/            # only with share entries
└── deployments/
    ├── 1751392700000/
    └── 1751392800000/
```

Sync, install, check, and start output is captured per release in `logs/`,
retained like releases, so a failed deployment can be examined after the
fact; failure messages cite the log path.

Run one tinycd per project; any number can run side by side because every
project has its own root, and tinycd holds an exclusive lock on `<root>/lock`
so a second instance pointed at the same root refuses to start. Instances
that listen for webhooks each need their own `hook` address.

The sync command starts in a new empty deployment folder. `TINYCD_REPO` (and
`TINYCD_REF` when `ref` is set) is available only to that command. Install
and start run in the populated folder. The folder carries its final name from
the beginning — nothing is renamed after install, so absolute paths that
package managers bake into virtualenvs and shebangs stay valid — and a marker
file distinguishes completed releases from interrupted ones, which are
cleaned up on the next deployment along with failed syncs and installs.
Successful releases beyond `keep` are deleted oldest-first after the new
release starts. `head` records the deployed commit so restarts pick up where
the last run stopped.

The start command runs in its own process group. When a new release is ready,
or when tinycd itself receives Ctrl+C, SIGTERM, or SIGHUP (the terminal that
started it closed), the whole group receives SIGTERM; anything still running
ten seconds later receives SIGKILL.

Create the interlock file to pause before syncing or restarting, then remove it
to continue:

```sh
touch "<root>/interlock"
rm "<root>/interlock"
```

Interlock access errors fail closed.

## Windows

Commands run through `cmd /C` by default; point `shell` at PowerShell or Git
Bash to change that. The default sync command uses `"%TINYCD_REPO%"`, and
`git` must be on `PATH`.

`current` is an NTFS junction, so neither administrator rights nor Developer
Mode are needed. Every process a release spawns is tracked by a job object.
Stopping a release sends Ctrl+Break when the release shares tinycd's console,
waits ten seconds, then terminates everything left in the job. If tinycd itself
dies, Windows closes the job and stops the release with it; starting tinycd
again restarts the release from `current`.

The config file permission check for `token` is Unix-only, so on Windows
prefer the `TINYCD_TOKEN` environment variable.

## Agent skills

The [skills/](skills/) directory ships skills for Claude Code and compatible
agents: `tinycd-setup` walks through setting up deployments end to end, and
`tinycd-configuration` documents every option, the precedence rules, and
common misconfigurations. Copy them into your agent's skills directory or
point it at this repository.
