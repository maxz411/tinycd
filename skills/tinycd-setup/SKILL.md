---
name: tinycd-setup
description: Set up tinycd to deploy a project, either from a Git URL or from a local checkout. Covers choosing an invocation mode, first deployment, webhooks, running as a service, restarts, pausing, manual rollback, running several projects side by side, and troubleshooting startup errors. Use when installing tinycd, adding a project to it, or diagnosing why a deployment did not start.
---

# Setting up tinycd

tinycd is a single binary that watches a Git remote and runs a small pipeline
whenever its HEAD changes:

```text
sync -> install -> start
```

Every deployment gets a fresh folder; `current` atomically points at the
newest started release, and older releases are retained for rollback.

## Prerequisites

- `git` on `PATH` (tinycd shells out to `git ls-remote` and `git clone`).
- Build with `cargo build --release`; the binary is `target/release/tinycd`.
- The project needs a `start` command tinycd can run (see the
  `tinycd-configuration` skill). tinycd never guesses how to launch a project.

## Choose an invocation mode

**Deploy a Git URL** when the machine only hosts the app:

```sh
tinycd git@github.com:example/app.git ~/apps/example
```

- The directory defaults to the current one and is created if missing.
- Deployments live directly in that directory: `current`, `head`, `lock`,
  `deployments/`.
- The deployment recipe (`install`, `start`, `shell`, `[env]`) is read from
  the `tinycd.toml` committed to the repository, re-read from every synced
  release, so pushing a config change takes effect on the next deployment.
- tinycd writes a two-line `tinycd.toml` into the directory recording the URL,
  so a plain `tinycd` run there later resumes the same deployment. It never
  overwrites a file the user authored (only files starting with
  `# Written by tinycd` are refreshed).

**Track a local checkout** when the machine is also a place to work:

```sh
tinycd            # inside the checkout
tinycd ~/code/app # or point at it
```

- The path must be a Git checkout with an `origin` remote (or set `repo`).
- The checkout stays untouched; deployments go to `~/.tinycd/<name>-<hash>/`
  (`%USERPROFILE%\.tinycd\` on Windows), keyed by the checkout's absolute
  path so restarts find the same releases.
- Commit and push from the checkout, or push from anywhere else — polling
  watches the remote, not the working tree, so both deploy. Uncommitted local
  changes are never deployed.

A first run deploys immediately; there is no separate "init" step. The
startup output prints which repository is tracked and where the root is.

Anything that looks like `scheme://…` or scp-style `host:path` is treated as
a URL; anything else must be an existing directory. `--repo` cannot be
combined with a URL argument.

## Verify a deployment

```sh
tinycd <url-or-path> --poll 5   # short poll while testing
```

Watch for `running sync/install/start` lines and a final
`deployed <root>/deployments/<id>`. Then check:

- `<root>/current` points at the newest release folder.
- The app answers (curl its port, check its log file, etc.).
- Push a trivial commit and confirm a second release appears and `current`
  moves.

## Webhooks

Polling is the default (30s). For push-triggered deploys, add a hook:

```sh
export TINYCD_TOKEN="$(openssl rand -hex 32)"   # >= 32 printable ASCII chars
tinycd <source> --hook 127.0.0.1:8080

curl -X POST -H "Authorization: Bearer $TINYCD_TOKEN" \
  http://127.0.0.1:8080/deploy
```

- Hook mode requires the token; prefer the environment variable over the
  config file or the CLI flag.
- A webhook POST always deploys, even if the remote is unchanged — it doubles
  as a redeploy button.
- With both `hook` and `poll` set, polling skips commits a webhook already
  deployed. Setting only `hook` disables polling.
- Put HTTPS in front (reverse proxy) whenever the hook crosses an untrusted
  network. Each tinycd instance needs its own hook address.

## Run as a service

tinycd is a long-running foreground process; use the platform's service
manager. systemd example:

```ini
[Unit]
Description=tinycd for example-app
After=network-online.target

[Service]
ExecStart=/usr/local/bin/tinycd /srv/example-app
Restart=on-failure
User=deploy
Environment=TINYCD_TOKEN=…   # only if using webhooks

[Install]
WantedBy=multi-user.target
```

On macOS use a launchd agent with `KeepAlive`; on Windows use Task Scheduler
or a service wrapper. On restart, tinycd first starts the release `current`
points at, then deploys anything pushed while it was down (the `head` file
records the last deployed commit).

## Pause, stop, and roll back

- **Pause**: create the interlock file (`<root>/interlock` by default);
  tinycd waits before syncing or restarting until it is removed. Access
  errors on the interlock fail closed.
- **Stop**: Ctrl+C or SIGTERM. The release's whole process group gets SIGTERM
  (Ctrl+Break on Windows), then SIGKILL after a 10-second grace period. On
  Windows a job object guarantees nothing outlives tinycd.
- **Roll back**: the reliable path is `git revert` + push — tinycd deploys the
  revert like any commit. For a manual rollback: stop tinycd, repoint
  `<root>/current` at an older folder under `<root>/deployments/`, start
  tinycd; it resumes whatever `current` points at and will not redeploy until
  the remote actually changes.

## Multiple projects

Run one tinycd per project; any number run side by side. Each project has its
own root, and tinycd holds an exclusive lock on `<root>/lock`, so a second
instance pointed at the same root exits immediately instead of interfering.

## Troubleshooting startup errors

| Error | Meaning / fix |
|---|---|
| `another tinycd is already managing <root>` | A live instance holds `<root>/lock`. Find it (`pgrep -fl tinycd`) and stop it, or you meant a different project. |
| `<dir> is not a Git checkout with an origin remote` | Local mode needs `origin`, or set `repo` in `tinycd.toml`, or pass a Git URL instead. |
| `<arg> is neither an existing directory nor a Git URL` | Typo in the path, or the URL lacks a scheme/`host:path` shape. |
| `start is not configured; …` (at deploy time) | No `start` on the CLI, in the local file, or in the repository's `tinycd.toml`. Commit one to the repo or pass `--start`. |
| `…contains a token and must not be readable by group or other users` | `chmod 600 tinycd.toml`, or move the token to `TINYCD_TOKEN`. |
| `failed to listen on <address>` | Another process (often another tinycd) owns that hook port; pick a unique address per instance. |
| `hook mode requires --token, …` | Webhooks never run unauthenticated; generate a token first. |
