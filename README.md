# tinycd

`tinycd` is a tiny self-hosted deployment platform: one binary that is the
dashboard, the host that runs your releases, and the command line that
drives both. Point it at a repository and every push deploys:

```text
sync -> build -> start -> verify -> switch
```

Each deployment gets a new folder or image. Older releases are retained for
rollback, and `current` is atomically moved to the newest verified release.
Releases run as plain processes or, when a container engine is available, as
containers. A GitHub App created from the dashboard turns pushes into
deployments and reports back to the commit.

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
so they run on any distribution. `git` must be on `PATH` at runtime, and
`docker`, `podman`, or `nerdctl` only if releases should run as containers.
To build from source instead, run `cargo install --path .` from a checkout.

## Quick start

Commit a `.tinycd/config.toml` to the project being deployed:

```toml
install = "npm ci"
start = "npm start"
port = 3000
```

Then run tinycd inside a checkout, or point it at a Git URL and a directory
to deploy into:

```sh
tinycd                                       # deploy this checkout
tinycd git@github.com:example/app.git ~/apps/example
```

If the checkout has no config yet and the terminal is interactive, a setup
wizard walks through creating one: detected remote, process or container,
port, suggested install and start commands, and where releases should live.

tinycd then starts the dashboard at <http://127.0.0.1:4000> if none is
running, registers the project with it, deploys, and shows a live terminal
view. Press `q` to leave the terminal view. When the dashboard was started
by this command it stops with it; when a dashboard was already running, as a
service or in another terminal, the project simply stays registered there.

Open the dashboard in a browser, choose a password, and everything else is
there: projects, hosts, logs, deploy, roll back, settings.

To make this machine a proper deployment host that survives reboots, run:

```sh
tinycd init
```

It sets the dashboard password, installs tinycd as a service (systemd,
launchd, or a scheduled task), and optionally creates the GitHub App. Every
step is also available later in the dashboard's Settings page.

## How it fits together

```text
                         GitHub (push webhooks, installation tokens, statuses)
                                         │
   ┌─────────────── dashboard (hub) ─────┴──────────┐
   │  records · scheduler · web UI · JSON API        │
   │  ┌───────────────┐                              │
   │  │ local host    │  ◀── in-process channel      │
   │  └───────────────┘                              │
   └──────────────▲───────────────────▲──────────────┘
                  │ WebSocket, TLS     │
           ┌──────┴──────┐      ┌──────┴──────┐
           │  host: pi   │      │  host: vps  │      each host runs cartons:
           └─────────────┘      └─────────────┘      processes or containers
```

- **The hub** keeps the records, decides what to deploy and when, and serves
  the web dashboard and the API. Its state is small JSON files in a data
  directory (`~/.tinycd`, `/var/lib/tinycd` as root, or `$TINYCD_HOME`).
- **A host** (an agent) runs cartons on one machine and takes orders from
  the hub. One host lives inside the hub's own process, so a single machine
  needs nothing else. More hosts connect *out* to the hub over a WebSocket,
  so they can sit behind NAT.
- **A carton** is one running release. The pipeline builds and starts
  cartons through a runtime and never cares which: `process` runs the start
  command directly; `container` builds an image and runs it through any
  docker-compatible command line (`docker`, `podman`, `nerdctl`).
- **The links** carry the same messages whether the host is in the hub's
  process, in a container next to it, or on another machine. Remote links
  use TLS with the hub's certificate pinned by fingerprint, so a self-signed
  hub is as safe as one behind a real certificate.
- **The interfaces** — the web dashboard, the terminal dashboard, and every
  `tinycd <verb>` — are all clients of the same API, so they work the same
  against a hub in this process, in a service, or in a container.

## Runtimes

Every project picks a runtime per host. Hosts report which runtimes they
have, and the wizard picks `container` only when an engine answers.

**process** runs the start command as a child of the host in its own
process group (a job object on Windows). Nothing to install. This is how
tinycd has always run releases, and it remains the default.

**container** turns the release into an image and runs it detached with
`--restart unless-stopped`, so it comes back with the machine and outlives
the host process; a returning host adopts it instead of restarting it. If the
repository has a `Dockerfile` and no install command, it is built as is;
otherwise tinycd generates one from the recipe:

```dockerfile
FROM node:22-slim            # `base`, or a guess from the project's files
WORKDIR /app
COPY . /app
RUN npm ci                   # install
ENV PORT=3000
EXPOSE 3000                  # port
CMD ["sh","-c","npm start"]  # shell + start
```

The `[env]` table is applied when the container runs, never baked into
image layers. `port` is published on the host as the same port and passed as
`PORT`. `share` entries become bind mounts from `<root>/shared/`. The `check`
command runs on the host, so `curl -sf localhost:$PORT/health` works for
both runtimes. Retention keeps the newest `keep` images; rollback runs an
older image again without rebuilding.

Set `TINYCD_OCI=podman` (or any command line) on a host to choose the
engine explicitly; otherwise the first of `docker`, `podman`, `nerdctl` whose
daemon answers is used.

## GitHub

Polling is the default trigger and works everywhere, including behind NAT:
every 30 seconds the remote head is compared with the last deployed commit.
A GitHub App adds what polling cannot:

- pushes deploy immediately through one webhook for every installed
  repository, when the dashboard has a public URL;
- private repositories clone with short-lived installation tokens, so no
  deploy keys need to live on hosts;
- each deployment is reported as a commit status, visible on pull requests.

The app is created from the dashboard through GitHub's manifest flow:
Settings → GitHub → Create GitHub App (or `tinycd github connect`). GitHub
shows the app, you press Create, and GitHub sends the credentials back to
the dashboard. You own the app; nothing goes through a third party. Then
install it on the repositories to deploy. The app is used for any project
whose repository URL is on github.com, over SSH or HTTPS, because tinycd
clones over HTTPS with the token whenever the app covers the repository.

Without a public URL the app's webhook stays inactive and polling carries
on, using the app's token for private repositories. Set the public URL in
Settings later and the webhook is pointed at it. If the app was created
without a public URL, enable **Active** in its webhook settings on GitHub as
well; GitHub's API cannot change that switch. For the webhook to be
reachable, put a reverse proxy with a real certificate in front of the
dashboard's plain HTTP port.

Forges without an app, and any other automation, can use a project's own
webhook: set a webhook token on the project and `POST /api/hooks/<project>`
with the token as a bearer, as GitLab's secret token (`X-Gitlab-Token`), or
as GitHub's webhook secret (`X-Hub-Signature-256` over the body). An
authorized POST always deploys, so it doubles as a redeploy button.

## More hosts

Add a host in the dashboard (Hosts → Add) or with `tinycd hosts add <name>`.
Either prints one command for the new machine:

```sh
tinycd agent https://192.168.1.43:4443 --token tcj1.<fingerprint>.<secret>
```

The join token is valid for a day and carries the dashboard's certificate
fingerprint, so the agent verifies the hub end to end before presenting the
token, then swaps it for a permanent one it keeps in its data directory.
Adding the first host also turns on the hub's HTTPS listener (`0.0.0.0:4443`
by default; change `secure_bind` in the data directory's `settings.json` or
pass `tinycd serve --secure-bind`). Afterwards the agent reconnects on its
own and is happy to be installed as a service:

```sh
tinycd service install --agent
```

Then assign projects to the host in the dashboard. Join commands connect to
the built-in HTTPS listener, whose certificate is pinned by the token;
expose that listener to the host separately from any dashboard reverse proxy. Each host builds what it
runs, so nothing has to be shipped between machines and no registry is
needed.

The dashboard and hosts run equally well in containers. The image at
[packaging/docker](packaging/docker/) ships the same binary with `git` and
the docker command line; mount the engine's socket so an agent inside the
container can run cartons through it. `packaging/docker/compose.yml` starts
a dashboard that way, and optionally a second host as another container.
Compose bind-mounts the data directory at the same absolute path inside and
outside the container, so the engine can also mount shared files into
releases. Set `TINYCD_DATA_DIR` to an absolute path to change it (default:
`~/.tinycd-container`). The directory must exist on the engine's host.

## Command line

```text
tinycd [SOURCE] [DIR] [flags]   deploy a checkout or URL (the classic form)
tinycd serve                    run the dashboard and the local host
tinycd agent <url> --token …    join a dashboard from this machine
tinycd init                     password, service, GitHub App

tinycd dashboard [project]      live terminal view of the running dashboard
tinycd status [project]         deployed release, commit, log path, daemon state
tinycd projects                 list projects
tinycd deploy [project]         deploy the newest commit now, and follow it
tinycd rollback [project] [--to ID]
tinycd restart | stop [project]
tinycd dry-run [project]        build and verify a throwaway release
tinycd logs [project] [--deployment ID] [-f]
tinycd remove <project> [--keep-running]
tinycd hosts [add <name>]
tinycd github connect | status
tinycd service install [--agent]
```

`project` defaults to the project for the current directory, or the only
project. `--data <DIR>` (or `TINYCD_HOME`) selects the data directory for
every command. `serve --bind` sets the plain HTTP listener, `--no-tui` gives
line output instead of the terminal view, and `--no-agent` runs a dashboard
that only uses joined hosts.

The classic form still accepts its flags — `--poll`, `--ref`, `--share`,
`--check`, `--root`, `--shell`, `--env`, `--sync`, `--install`, `--start`,
`--keep`, `--config`, `--token`, `--dry-run`, `--status`, `--rollback` — plus
`--runtime`, `--port`, `--base`, and `--dockerfile`. `--hook` now means
"accept the project's webhook at the dashboard" and needs a token; the
address it once took is ignored.

## Configuration

tinycd reads up to two config files: the local `.tinycd/config.toml` in the
tracked directory (or the file named by `--config`), and the
`.tinycd/config.toml` inside the repository, re-read from every synced
release. The repository's file provides the recipe — `shell`, `env`,
`install`, `start`, `check`, `port`, `base`, `dockerfile` — so a project
carries its own deployment recipe and changes take effect on the next
deployment. Everything else, what to watch and where to keep releases, only
comes from the machine running tinycd: the local file for the classic form,
or the project's settings in the dashboard.

All settings can be placed in the local `.tinycd/config.toml`:

```toml
name = "example"
repo = "git@github.com:example/app.git"
ref = "production"
poll = 30
runtime = "container"
port = 3000
root = "."
interlock = "interlock"
share = ["data/", ".env"]
shell = ["sh", "-c"]
sync = 'git clone --depth 1 --branch "$TINYCD_REF" "$TINYCD_REPO" .'
install = "npm ci"
start = "npm start"
check = "curl -sf localhost:3000/health"
base = "node:22-slim"
keep = 5

[env] # keep this table last: keys below a table header belong to it
NODE_ENV = "production"
```

See [config.example.toml](config.example.toml) for a copyable file. CLI
values override the local file, the local file (or the dashboard's project
settings) overrides the repository's file, and both override built-in
defaults; `--env KEY=VALUE` entries override matching `[env]` keys. Relative
paths in a local file are resolved relative to the file's directory —
`.tinycd/` — so `root = "."` keeps releases inside `.tinycd/` itself.

The default files are optional. Passing `--config <path>` makes that specific
file required. Unknown settings and invalid values are rejected, including a
tinycd setting accidentally placed under `[env]`.

`share` names paths that stay out of Git but must survive from release to
release — vendored libraries, `.env` files, SQLite databases, upload dirs.
Each entry is linked into the release from `<root>/shared/` after sync
(mounted into the container for the container runtime), so every release
sees the same files. A trailing slash marks an entry that should be created
as a directory when `<root>/shared/` does not have it yet.

`check` gates deployments: a started release must stay up for three seconds,
and pass the check command (retried for up to 30 seconds) when one is set,
before `current` moves. A release that fails is torn down and the previous
release is restarted, so a bad push degrades to a logged failure instead of
an outage. `tinycd dry-run` runs the same pipeline and verification against
a throwaway release and exits, without touching the deployed state.

The sync, install, check, and start commands run through `shell` and inherit
the host's environment plus the `[env]` table, and `PORT` when a port is set.
tinycd does not manage language runtimes or virtual environments; compose
the commands instead, or use the container runtime.

## Deployments

Each project's root on its host — printed at startup and shown in the
dashboard — is laid out as:

```text
<root>/
├── current -> deployments/1751392800000
├── head
├── logs/
│   └── 1751392800000.log
├── shared/            # only with share entries
└── deployments/
    ├── 1751392700000/
    └── 1751392800000/
```

For the classic form the root is `<dir>/.tinycd` when tinycd was given a
Git URL, and `<data>/<name>-<hash>` when it tracks a checkout. Projects
created in the dashboard live under `<data>/agent/<project>` on the local
host, or under the agent's own data directory elsewhere.

Sync, build, check, and start output is captured per release in `logs/`,
retained like releases, and streamed to the dashboard, which keeps its own
copy so failed deployments on any host can be examined from one place.

A burst of pushes during a slow build yields one more deployment of the
newest commit, not one per push. A deployment that fails is not retried
until the repository changes again or someone presses Deploy. Rollback,
restart, and stop are dashboard buttons and command-line verbs that act on
the live host; nothing needs to be stopped first. A release that exits on
its own is restarted with a growing delay unless it was stopped on purpose.

The start command runs in its own process group. When a new release is
ready, or when the host stops, the whole group receives SIGTERM; anything
still running ten seconds later receives SIGKILL. Containers are stopped the
same way for a new release, but keep running when the host process itself
stops; a returning host adopts them.

Create the interlock file to pause before syncing or restarting, then remove
it to continue:

```sh
touch "<root>/interlock"
rm "<root>/interlock"
```

Interlock access errors fail closed.

## Windows

Commands run through `cmd /C` by default; point `shell` at PowerShell or Git
Bash to change that. The default sync command uses `"%TINYCD_REPO%"`, and
`git` must be on `PATH`. The container runtime works through Docker Desktop's
`docker` command; the process runtime needs nothing.

`current` is an NTFS junction, so neither administrator rights nor Developer
Mode are needed. Every process a release spawns is tracked by a job object.
Stopping a release sends Ctrl+Break when the release shares tinycd's console,
waits ten seconds, then terminates everything left in the job. If tinycd
itself dies, Windows closes the job and stops the release with it; starting
tinycd again restarts the release from `current`.

`tinycd service install` prints the scheduled-task command to run at logon.

## Upgrading from 0.4

Everything from 0.4 keeps working: the classic invocation, both config
files, the release layout, and existing releases under
`~/.tinycd/<name>-<hash>` are picked up as they are. What changed:

- `tinycd` in a checkout now also starts the dashboard on
  <http://127.0.0.1:4000>, and attaches to one that is already running
  instead of refusing to start a second instance.
- `--rollback` needs the dashboard running (it is a live operation now) and
  no longer requires stopping anything.
- `--hook <address>` no longer opens a per-project listener; the dashboard
  accepts every project's webhook at `/api/hooks/<project>`. The address is
  ignored and the token is still required.
- Failed deployments are no longer retried on every poll.

## Agent skills

The [skills/](skills/) directory ships skills for Claude Code and compatible
agents: `tinycd-setup` walks through setting up deployments end to end, and
`tinycd-configuration` documents every option, the precedence rules, and
common misconfigurations. Copy them into your agent's skills directory or
point it at this repository.
