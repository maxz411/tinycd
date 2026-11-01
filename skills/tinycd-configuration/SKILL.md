---
name: tinycd-configuration
description: Reference for every tinycd configuration option (name, repo, ref, poll, hook, token, runtime, port, base, dockerfile, root, interlock, share, shell, sync, install, start, check, keep, env) — where each may be set, precedence between the CLI, the local .tinycd/config.toml or the dashboard's project settings, and the repository's .tinycd/config.toml, path and environment resolution rules, hub-level settings, and worked examples. Use when writing or debugging a .tinycd/config.toml, filling the project form, choosing CLI flags, or explaining why a setting did not take effect.
---

# tinycd configuration reference

## Where settings come from

Project settings merge from four layers; the first layer that sets a value
wins:

1. **CLI flags** of the classic invocation (and `TINYCD_TOKEN`).
2. **The project's own settings**: the local `.tinycd/config.toml` in the
   tracked directory (or the file named by `--config`, which makes it
   required) for the classic invocation, or the project form in the
   dashboard for projects created there. Both produce the same record.
3. **The repository's `.tinycd/config.toml`**, read from each freshly synced
   release — but only for the recipe: `shell`, `env`, `install`, `start`,
   `check`, `port`, `base`, `dockerfile`.
4. **Built-in defaults.**

The split in layer 3 is deliberate: a repository decides *how it builds and
runs*, while the machine running tinycd decides *what to watch, where, and
how*. Launcher-level keys (`name`, `repo`, `ref`, `poll`, `hook`, `token`,
`runtime`, `root`, `interlock`, `share`, `sync`, `keep`) inside a
repository's copy are ignored — they only apply when that file is the local
file.

Rules that apply everywhere:

- Unknown keys and invalid values are rejected, including in the repository's
  file (a typo there fails the deployment with a parse error).
- Relative paths in a file resolve against the file's directory; relative CLI
  paths resolve against the working directory.
- In TOML, keys written below a table header belong to that table — keep
  `[env]` last in the file, or use dotted keys (`env.PYTHONPATH = "..."`)
  anywhere. A tinycd setting name appearing inside `[env]` is rejected with a
  hint.
- `--env KEY=VALUE` entries override matching `[env]` keys; project `[env]`
  keys override the repository's.
- `PORT` is added to the environment of install, check, and start when
  `port` is set and `[env]` does not define it.

## Options

### name — display name
String. Defaults to the directory name (classic form) or the id. The project
id is derived from it (`my-app`) in the dashboard, or from the directory
name plus a short hash of its path in the classic form, so restarts find the
same releases.

### repo — what to watch
String: a Git URL or the name of a remote in the tracked checkout.
Default: the tracked checkout's `origin`. A URL passed as the first CLI
argument takes this role and cannot be combined with `--repo`. Remote names
are expanded once at startup. Polling runs `git ls-remote <repo> HEAD`, so
whatever git authentication works for that URL on the host works here. When
the GitHub App covers a github.com repository, clones and polls go over
HTTPS with an installation token instead, whatever the URL's scheme.

### ref — branch or tag to deploy
String, default: the remote HEAD. Feeds polling, the webhook match (a push
to another branch is ignored), and the default sync (`--branch
"$TINYCD_REF"`). A name that is both a branch and a tag resolves to the
branch.

### poll — polling interval, seconds
Integer ≥ 1, or `off`/unset in the dashboard. Default: 30 when neither
`poll` nor `hook` is set; unset when only `hook` is set. Each tick compares
the remote head with the last deployed or attempted commit and deploys on
change. A failed deployment is not retried until the remote changes again or
someone deploys by hand.

### hook — accept the project's webhook
Boolean (`true`), or any non-false value for compatibility with the old
listen address, which is ignored. Enables `POST /api/hooks/<project>` on the
dashboard, guarded by `token`; an authorized POST always deploys, even when
the remote is unchanged. In the dashboard, setting a webhook token is what
enables it.

### token — webhook secret
At least 32 printable non-space ASCII characters. Prefer `TINYCD_TOKEN` in
the environment: a token in `.tinycd/config.toml` requires the file to be
unreadable by group/other on Unix (`chmod 600`), and `--token` puts it in the
process list. Three delivery schemes are accepted: `Authorization: Bearer`,
GitLab's `X-Gitlab-Token`, and GitHub's `X-Hub-Signature-256` (the HMAC of
the body). All comparisons are constant-time. The GitHub App's own webhook
uses the app's secret instead and needs no per-project token.

### runtime — process or container
`"process"` (default) or `"container"` (also accepted: `oci`, `docker`,
`podman`). The host must report the runtime; the dashboard refuses a
container project on a host without an engine. Switching runtime takes
effect on the next deployment.

### port — the application's port
Integer. Optional for processes (it is only passed as `PORT` and used by
`check`); effectively required for containers, which publish it on the host
as the same port. A container project without a port runs unpublished, which
suits workers and bots.

### base — base image (container runtime)
String such as `node:22-slim`. Default: a guess from the project's files
(`package.json` → node, `Cargo.toml` → rust, `go.mod` → golang,
`requirements.txt`/`pyproject.toml` → python, `Gemfile` → ruby,
`composer.json` → php, else `debian:stable-slim`). Used only when tinycd
generates the Dockerfile.

### dockerfile — build the repository's own Dockerfile
Path relative to the repository. Default: `Dockerfile` when one exists and
no `install` is set; otherwise tinycd generates `Dockerfile.tinycd` from the
recipe (`FROM base`, `COPY . /app`, `RUN install`, `CMD shell start`). The
generated image applies `[env]` at run time, never in layers.

### share — paths every release shares
Array of relative paths, empty by default. After sync, each entry is linked
into the release from `<root>/shared/<entry>` (symlinks on Unix; junctions
for directories and hard links for files on Windows), or mounted into the
container at `/app/<entry>`. Missing sources are created — entries with a
trailing slash as directories, others as empty files.

### root — deployment state directory (local host only)
Path. Default: `<dir>/.tinycd` when tinycd was given a Git URL;
`<data>/<name>-<hash>` when tracking a checkout; `<data>/agent/<project>`
for projects created in the dashboard. Contains `current`, `head`, `logs/`,
`shared/`, and `deployments/`. Relative values resolve against the config
file's directory, so `root = "."` keeps releases inside `.tinycd/`. Hosts on
other machines ignore it and use their own data directory.

### interlock — pause file
Path. Default: `<root>/interlock`. While the file exists, the host waits
before syncing and before restarting the release, checking once a second.
Errors reading it fail closed.

### shell — command interpreter
Array, e.g. `["sh", "-c"]` (default) or `["cmd", "/C"]` (Windows default).
On the CLI, repeat the flag per argument; in the dashboard, space-separated.
Runs `sync`, `install`, `check`, and `start`, each as a single string
appended as the final argument, and becomes the `CMD` of a generated
Dockerfile. May come from the repository's file.

### sync — fetch a release
Command string. Default: `git clone --depth 1 "$TINYCD_REPO" .`
(`"%TINYCD_REPO%"` on Windows; `--branch "$TINYCD_REF"` when `ref` is set).
Runs in a brand-new empty release folder; `TINYCD_REPO` and `TINYCD_REF` are
set only for this command, and a GitHub App credential arrives through
`GIT_CONFIG_*` so any git underneath picks it up. Launcher-level: the
repository's copy cannot change it.

### install — build a release (optional)
Command string, disabled by default. For processes it runs in the release
folder after sync; for containers it becomes the image's `RUN` step. A
non-zero exit discards the release and keeps the previous one running. May
come from the repository's file.

### start — run the release (required)
Command string, no default. Runs in the release folder in its own process
group, or as the container's command. Must be resolvable by deploy time from
the project settings or the repository's file. When a new release is ready
the group gets SIGTERM (Ctrl+Break on Windows), then SIGKILL after 10
seconds; containers get `stop --time 10`. Long-running foreground commands
only.

### check — health gate (optional)
Command string, disabled by default; may come from the repository's file.
Every started release must stay up for three seconds; with `check` set it
must also make this command exit 0 (run on the host in the release folder,
retried every second for up to 30 seconds) before `current` advances. On
failure the new release is torn down and the previous release is restarted.
`tinycd dry-run` applies the same verification to a throwaway release.

### keep — retained releases
Integer ≥ 1, default 5. After a successful start, older releases (and
images) beyond the newest `keep` are deleted, oldest first, along with
leftovers of interrupted deployments. The live release is never pruned.

### [env] — extra environment
Table of `KEY = "value"`. Applied to sync, install, check, and start (and to
`docker run`), layered: repository `[env]` < project `[env]` < `--env
KEY=VALUE`. `TINYCD_TOKEN`, `TINYCD_REPO`, and `TINYCD_REF` are always
removed from install, check, and start environments.

## Hub-level settings

Kept in `<data>/settings.json` and edited in the dashboard's Settings page:

- `bind` — the plain HTTP listener (`127.0.0.1:4000`); `tinycd serve --bind`
  overrides it for one run.
- `secure_bind` — the HTTPS listener for agents and remote browsers; set
  automatically to `0.0.0.0:4443` when the first host is added.
- `public_url` — where GitHub and other machines reach the dashboard; used
  for the GitHub App's webhook and the links in commit statuses.
- `poll` — the default interval for new projects.
- `github` — the app's credentials, written by the manifest flow.
- `admin_password` — a PBKDF2 hash; set on first visit or by `tinycd init`.

Files never meant to be edited: `hosts.json`, `projects/*.json`,
`deployments/`, `logs/`, `api-token` (the CLI's bearer token, 0600),
`tls/` (the self-signed certificate agents pin), `daemon.json` and `lock`
(the running hub).

## Worked examples

Node app as a container, config committed to the repository:

```toml
install = "npm ci"
start = "npm start"
port = 3000
check = "curl -sf localhost:$PORT/health"
```

Python app with a fixed virtualenv on the host (local file, tracking a
checkout, process runtime):

```toml
install = "/home/me/app-venv/bin/pip install -r requirements.txt"
start = "/home/me/app-venv/bin/python -m uvicorn app:app --port 8000"
port = 8000

[env]
PYTHONPATH = "/home/me/code"
```

Repository with its own Dockerfile, deployed to a joined host (dashboard
form): runtime `container`, port `8080`, leave install empty so the
Dockerfile is used, `dockerfile = "deploy/Dockerfile"` if it is not at the
root.

Webhook-only deployment with full clone and longer retention (local file):

```toml
hook = true
sync = 'git clone "$TINYCD_REPO" .'
keep = 10
# token via TINYCD_TOKEN in the service environment
```

## Debugging "my setting didn't take effect"

1. Which layer set it? Confirm with the value on the CLI or in the project
   form, then walk the precedence chain downward.
2. Launcher keys in the repository's copy are ignored by design — move
   `poll`, `hook`, `root`, `keep`, `sync`, `runtime`, etc. to the project
   settings.
3. Repository recipe changes apply to the *next* deployment, not the running
   release; push a commit or press Deploy.
4. Paths looking wrong: file-relative vs CLI-relative resolution.
5. `[env]` seemingly missing: check it is the last table in the file and that
   a higher layer isn't overriding the key. In containers, `[env]` applies at
   run time, not during `install`.
6. `check` failing for a container: it runs on the host, so use the
   published port (`$PORT`), not a container-internal address.
