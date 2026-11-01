---
name: tinycd-setup
description: Set up tinycd to deploy a project — from a Git URL, a local checkout, or the web dashboard — as a process or a container, on the machine running the dashboard or on a joined host. Covers the classic invocation, the dashboard and its password, running as a service, the GitHub App, adding hosts, webhooks, rollback, dry runs, logs, and troubleshooting startup and deployment errors. Use when installing tinycd, adding a project or host, or diagnosing why a deployment did not go live.
---

# Setting up tinycd

tinycd is a single binary that is three things: the **hub** (dashboard,
records, scheduler, JSON API), a **host** (an agent that runs releases on
one machine), and the **command line** that drives both. A release runs as a
**carton**: a plain process, or a container through docker, podman, or
nerdctl. The pipeline is the same either way:

```text
sync -> build -> start -> verify -> switch
```

Every deployment gets a fresh folder (and, for containers, an image);
`current` atomically points at the newest verified release, and older
releases are retained for rollback.

## Prerequisites

- `git` on `PATH` (tinycd shells out to `git ls-remote` and `git clone`).
- For container releases: `docker`, `podman`, or `nerdctl` whose daemon
  answers. `TINYCD_OCI=podman` picks one explicitly.
- Build with `cargo build --release`; the binary is `target/release/tinycd`.
- The project needs a `start` command (see `tinycd-configuration`); tinycd
  never guesses how to launch a project. Set `port` so containers publish it
  and health checks can reach it.

## The classic invocation

```sh
tinycd            # deploy this checkout
tinycd ~/code/app # or that checkout
tinycd git@github.com:example/app.git ~/apps/example   # deploy a URL into a directory
```

What happens:

1. In a checkout with no `.tinycd/config.toml`, on an interactive terminal,
   a full-screen wizard writes one: remote, process or container (container
   only offered when an engine answers), port, install and start commands,
   and where releases live. Non-interactive runs skip the wizard and fail
   with a clear error instead.
2. The project is registered with the dashboard: the one already running
   for the data directory, or one started in this process on
   `http://127.0.0.1:4000`.
3. If nothing is deployed yet, a deployment is queued; the terminal shows the
   live view of that project. `q` leaves the view. A dashboard started by
   this command stops with it (process releases stop; containers keep
   running under their engine); a dashboard that was already running keeps
   the project.

A URL argument writes a small `.tinycd/config.toml` into the directory
recording the URL, so a plain `tinycd` there later resumes the same
deployment. It never overwrites a file the user authored.

Anything that looks like `scheme://…` or scp-style `host:path` is a URL;
anything else must be an existing directory. `--repo` cannot be combined
with a URL argument.

## The dashboard

`tinycd serve` runs the hub and the local host in the foreground (with the
terminal view on a terminal, plain lines with `--no-tui`). The data
directory is `$TINYCD_HOME`, `/var/lib/tinycd` as root, or `~/.tinycd`;
`--data <DIR>` overrides it for any command. The first visit to the web
dashboard asks for a password. From there: New project (any host, any
runtime, full recipe), Deploy now, Roll back, Restart, Stop, live logs,
Hosts, Settings.

`tinycd init` is the node wizard: password, public URL, service install, and
the GitHub App, in one go.

The command line is a client of the same API and needs the dashboard
running: `status`, `projects`, `deploy`, `rollback [--to ID]`, `restart`,
`stop`, `dry-run`, `logs [-f]`, `remove`, `hosts`, `github`, `dashboard`
(the terminal view against a running hub). `project` defaults to the project
for the current directory, or the only project.

## Run as a service

```sh
tinycd service install          # the dashboard and local host
tinycd service install --agent  # a joined host
```

Writes a systemd unit (system-wide as root, else a user unit; run
`loginctl enable-linger $USER` so it survives logout), a launchd agent on
macOS, or prints the `schtasks` command on Windows, and enables it. The
service runs `tinycd serve --no-tui` (or `agent`) with `TINYCD_HOME` set to
the data directory in use. On restart, hosts start whatever `current` points
at (adopting containers that are still running), then the hub deploys
anything pushed meanwhile.

## GitHub

Polling (every 30s by default) is the trigger that works everywhere. The
GitHub App adds push-triggered deploys (when the dashboard has a public
URL), private-repository clones with short-lived installation tokens, and
commit statuses.

1. Settings → GitHub → Create GitHub App, or `tinycd github connect
   [--org NAME]`. This is GitHub's manifest flow: the browser lands on
   GitHub's form, the user presses Create, GitHub redirects back to the
   dashboard with the credentials. The user owns the app.
2. Install the app on the repositories (link shown after creation, and in
   Settings).
3. Optional: set the public URL in Settings; the webhook is pointed at it.
   If the app was created without a public URL, also enable Active in its
   GitHub webhook settings; GitHub's API cannot activate the hook.
   Without one, polling continues and still uses the app's token for private
   repositories.

Projects whose `repo` is on github.com (SSH or HTTPS) are cloned over HTTPS
with the token whenever the app covers the repository; nothing changes in
their config.

## Webhooks without an app

Set a webhook token on the project (`--token`/`TINYCD_TOKEN` with `--hook`
in the classic form, or the field in the dashboard). Then
`POST /api/hooks/<project>` on the dashboard with the token as a bearer, as
GitLab's `X-Gitlab-Token`, or as the GitHub webhook secret
(`X-Hub-Signature-256`). Any authorized POST deploys, so it doubles as a
redeploy button. Put HTTPS in front whenever the request crosses an
untrusted network.

## More hosts

```sh
tinycd hosts add pi
# prints, for the other machine:
#   tinycd agent https://192.168.1.43:4443 --token tcj1.<fingerprint>.<secret>
```

The agent connects out (works behind NAT), verifies the hub's certificate
against the fingerprint in the token, redeems the one-time token for a
permanent one saved in its data directory, and reconnects on its own. Adding
the first host turns on the hub's HTTPS listener (`0.0.0.0:4443`; change
`secure_bind` in `settings.json` or `tinycd serve --secure-bind`). Then
assign projects to the host in the dashboard; each host builds what it runs.
A plain `http://` hub is refused for non-local hosts unless `--insecure`.

Both roles run in containers too: `packaging/docker/compose.yml` runs the
dashboard with the engine socket mounted so it can run container releases.

## Verify a deployment

Start with a dry run — it builds and starts one throwaway release, verifies
it stays up (and passes `check` if configured), stops it, and exits without
touching the deployed state:

```sh
tinycd dry-run [project]
```

Then `tinycd deploy` follows a real deployment to its end and exits non-zero
if it does not go live. `tinycd status` shows the release id, commit, log
path, and whether the daemon runs. Logs are captured per release on the host
and copied to the dashboard (`<data>/logs/<project>/<id>.log`);
`tinycd logs -f` follows one.

Releases are health-gated: a started carton must stay up three seconds, and
pass `check` when configured, before `current` advances — otherwise it is
torn down, the previous release is restarted, and the failure is recorded on
the deployment. Configure `check = "curl -sf localhost:$PORT/health"` for
real readiness gating; it runs on the host for both runtimes.

## Pause, stop, and roll back

- **Pause**: create the interlock file (`<root>/interlock` by default);
  the host waits before syncing or restarting until it is removed.
- **Stop**: `tinycd stop <project>` or the button. Stopping the hub or an
  agent process stops process releases with it; containers keep running and
  are adopted when the agent returns.
- **Roll back**: `tinycd rollback [project] [--to ID]` or the button on any
  retained release. Live operation; nothing needs stopping. The recorded head
  is left alone, so the rollback survives until the remote actually changes.
  For a permanent fix, `git revert` and push.
- **Crashes**: a release that exits on its own is restarted after 5, 10, 20,
  40, then 60 seconds, unless it was stopped on purpose.

## Troubleshooting

| Symptom | Meaning / fix |
|---|---|
| `no tinycd dashboard is running for <data>` | The verb needs the hub. Start `tinycd serve` (or `tinycd` in a checkout), or check `--data`/`TINYCD_HOME` points at the right directory. |
| `a tinycd dashboard is already running for <data>` | One hub per data directory. Attach with `tinycd dashboard`, or use another `--data`. |
| `host <name> is offline` | The project's host is not connected. Check the agent process and its log; it reconnects with backoff. |
| `host <name> has no container runtime` | The host reported no engine. Install docker/podman/nerdctl there, or set the project's runtime to process. |
| `<dir> is not a Git checkout with an origin remote` | Classic form needs `origin`, or set `repo`, or pass a URL. |
| `start is not configured; …` (at deploy time) | No `start` in the project settings or the repository's `.tinycd/config.toml`. |
| `…contains a token and must not be readable by group or other users` | `chmod 600 .tinycd/config.toml`, or move the token to `TINYCD_TOKEN`. |
| `hook mode requires --token, …` | Webhooks never run unauthenticated; generate a token first. |
| `the server exited with … within 3 seconds` / `the container exited …` | The start command crashed on launch; the previous release was restarted. Read the deployment's log, fix, push. |
| `the check command did not pass within 30 seconds` | The release never became healthy; same recovery. Check `port` and that the check uses `$PORT` or the right port. |
| `lost: the host reconnected without reporting on it` | The host lost the deployment (restarted mid-build). Deploy again. |
| `the hub's certificate does not match the fingerprint in the join token` | The hub's certificate changed (new data directory?). Mint a new join token. |
| `refusing to send the host token over plain HTTP` | Use the `https://` URL from `hosts add`, or `--insecure` on a trusted network. |
| Vendored/untracked files missing in releases | Fresh clones only contain committed files. List stateful paths in `share` and put the real files under `<root>/shared/`. |
