---
name: tinycd-configuration
description: Reference for every tinycd configuration option (repo, poll, hook, token, root, interlock, shell, sync, install, start, keep, env) — where each may be set, precedence between the CLI, the local tinycd.toml, and the repository's tinycd.toml, path and environment resolution rules, and worked examples for common stacks. Use when writing or debugging a tinycd.toml, choosing CLI flags, or explaining why a setting did not take effect.
---

# tinycd configuration reference

## Where settings come from

Settings merge from four layers; the first layer that sets a value wins:

1. **CLI flags** (and the `TINYCD_TOKEN` environment variable).
2. **The local `tinycd.toml`** in the tracked directory, or the file named by
   `--config` (which makes that file required instead of optional).
3. **The repository's `tinycd.toml`**, read from each freshly synced release —
   but only for the project recipe: `shell`, `env`, `install`, `start`.
4. **Built-in defaults.**

The split in layer 3 is deliberate: a repository decides *how it builds and
runs*, while the machine running tinycd decides *what to watch and where to
keep releases*. Launcher-level keys (`repo`, `poll`, `hook`, `token`, `root`,
`interlock`, `sync`, `keep`) inside a repository's copy are silently ignored —
they only apply when that file is the local file (i.e. when tracking a
checkout, where the working tree's `tinycd.toml` is the local file).

Rules that apply everywhere:

- Unknown keys and invalid values are rejected, including in the repository's
  file (a typo there fails the deployment with a parse error).
- Relative paths in a file resolve against the file's directory; relative CLI
  paths resolve against the working directory.
- In TOML, keys written below a table header belong to that table — keep
  `[env]` last in the file.
- `--env KEY=VALUE` entries override matching `[env]` keys; local `[env]`
  keys override the repository's.

## Options

### repo — what to watch
String: a Git URL or the name of a remote in the tracked checkout.
Default: the tracked checkout's `origin`. A URL passed as the first CLI
argument takes this role and cannot be combined with `--repo`. Remote names
are expanded to their URL once at startup; values that are not a known remote
are used verbatim. Polling runs `git ls-remote <repo> HEAD`, so whatever git
authentication works for that URL in the service's environment (ssh agent,
credential helper) works here.

### poll — polling interval, seconds
Integer ≥ 1. Default: 30 when neither `poll` nor `hook` is set; unset (no
polling) when only `hook` is set. Each tick compares the remote HEAD with the
last deployed commit and deploys on change. A fresh setup deploys on the
first tick.

### hook — webhook listener address
Socket address such as `127.0.0.1:8080`. Disabled by default. Enables
`POST /deploy` guarded by a bearer token; an authorized POST always deploys,
even when the remote is unchanged (useful as a redeploy button). May be
combined with `poll`; polling then skips commits the webhook already
deployed. Requires `token`.

### token — webhook bearer token
At least 32 printable non-space ASCII characters. Prefer `TINYCD_TOKEN` in
the environment: a token in `tinycd.toml` requires the file to be unreadable
by group/other on Unix (`chmod 600`), and `--token` puts it in the process
list. Only the local file's token is used; a `token` in the repository's copy
is ignored. tinycd stores and compares only a SHA-256 of it.

### root — deployment state directory
Path. Default: the deployment directory itself when tinycd was given a Git
URL; `~/.tinycd/<name>-<hash>` when tracking a checkout (name from the
directory, hash from its absolute path). Contains `current` (symlink on Unix,
junction on Windows), `head` (last deployed commit), `lock` (one instance per
root), and `deployments/`. Point two projects at the same root and the second
instance refuses to start.

### interlock — pause file
Path. Default: `<root>/interlock`. While the file exists, tinycd waits before
syncing and before restarting the server, checking once a second; remove it
to continue. Errors reading it fail closed (the deployment stops rather than
proceeding).

### shell — command interpreter
Array, e.g. `["sh", "-c"]` (default) or `["cmd", "/C"]` (Windows default).
On the CLI, repeat the flag per argument: `--shell powershell --shell
-Command`. Runs `sync`, `install`, and `start`, each as a single string
appended as the final argument. May come from the repository's file.

### sync — fetch a release
Command string. Default: `git clone --depth 1 "$TINYCD_REPO" .`
(`"%TINYCD_REPO%"` on Windows). Runs in a brand-new empty staging folder;
`TINYCD_REPO` (the resolved repo URL) is set only for this command. Replace
it to fetch differently (full clone, submodules, artifact download).
Launcher-level: the repository's copy cannot change it, since sync runs
before that file exists locally.

### install — build a release (optional)
Command string, disabled by default. Runs in the staging folder after sync,
after the repository's `tinycd.toml` has been read. A non-zero exit discards
the staging folder and keeps the previous release running. May come from the
repository's file.

### start — run the release (required)
Command string, no default. Runs in the release folder in its own process
group. Must be resolvable by deploy time from the CLI, the local file, or the
repository's file — otherwise the deployment fails with a message naming all
three. When a new release is ready (or tinycd stops), the group gets SIGTERM
(Ctrl+Break on Windows), then SIGKILL after 10 seconds. Long-running
foreground commands only; if the command daemonizes itself, tinycd cannot
manage it.

### keep — retained releases
Integer ≥ 1, default 5. After a successful start, older releases beyond the
newest `keep` are deleted, oldest first, along with leftovers of interrupted
deployments. The live release is never pruned.

### [env] — extra environment
Table of `KEY = "value"`. Applied to sync, install, and start, layered:
repository `[env]` < local `[env]` < `--env KEY=VALUE`. `TINYCD_TOKEN` and
`TINYCD_REPO` are always removed from install and start environments. tinycd
does not manage language runtimes or virtualenvs — encode them as absolute
paths in the commands or here.

## Worked examples

Node app, config committed to the repository:

```toml
install = "npm ci"
start = "npm start"
```

Python app with a fixed virtualenv on the host (local file, tracking a
checkout):

```toml
install = "/home/me/app-venv/bin/pip install -r requirements.txt"
start = "/home/me/app-venv/bin/python -m uvicorn app:app --port 8000"

[env]
PYTHONPATH = "/home/me/code"
```

Hook-only deployment with full clone and longer history retention (local
file, launcher side):

```toml
hook = "127.0.0.1:8080"
sync = 'git clone "$TINYCD_REPO" .'
keep = 10
# token via TINYCD_TOKEN in the service environment
```

## Debugging "my setting didn't take effect"

1. Which file set it? Run with the value on the CLI to confirm the setting
   itself works, then walk the precedence chain downward.
2. Launcher keys in the repository's copy are ignored by design — move
   `poll`, `hook`, `root`, `keep`, `sync`, etc. to the local file or CLI.
3. Repository recipe changes (`install`, `start`, `shell`, `[env]`) apply to
   the *next* deployment, not the running release; push a commit or POST to
   the hook to redeploy.
4. Paths looking wrong: file-relative vs CLI-relative resolution (see rules
   above).
5. `[env]` seemingly missing: check it is the last table in the file and that
   a higher layer isn't overriding the key.
