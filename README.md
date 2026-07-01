# tinycd

`tinycd` runs a small deployment pipeline when a Git remote changes, when it
receives a webhook, or both:

```text
sync -> install -> start
```

Each deployment gets a new folder. Older deployments are retained for rollback,
and `current` is atomically moved to the newest launched release.

## Quick start

Create `tinycd.toml` in the Git project:

```toml
install = "npm ci"
start = "npm start"
```

Then run:

```sh
tinycd
```

With no other configuration, tinycd reads the repository's `origin`, polls it
every 30 seconds, shallow-clones new releases, keeps five releases, and uses
`.tinycd/interlock` as the deployment interlock. A fresh polling setup deploys
immediately. After a restart, tinycd starts the existing `current` release,
then deploys anything that was pushed while it was down.

The defaults are:

```toml
poll = 30
root = ".tinycd"
interlock = ".tinycd/interlock"
sync = 'git clone --depth 1 "$TINYCD_REPO" .'
install = "true"
keep = 5
```

`repo` defaults to the current Git project's `origin`. `hook` is disabled by
default. `start` has no default because guessing how to launch an arbitrary
project would be unsafe.

## Configuration

All runtime settings can be placed in `tinycd.toml`:

```toml
repo = "git@github.com:example/app.git"
poll = 30
hook = "127.0.0.1:8080"
root = ".tinycd"
interlock = ".tinycd/interlock"
sync = 'git clone --depth 1 "$TINYCD_REPO" .'
install = "npm ci"
start = "npm start"
keep = 5
```

See [tinycd.example.toml](tinycd.example.toml) for a copyable file. CLI values
and `TINYCD_TOKEN` override the file. File values override built-in defaults.
Relative paths in the file are resolved relative to the file's directory.

The default file is optional. Passing `--config <path>` makes that specific
file required. Unknown settings and invalid values are rejected.

## Webhooks

Setting only `hook` enables hook-only mode. Setting both `hook` and `poll`
enables both modes; polling skips commits a webhook already deployed. A
webhook always deploys, even when the remote has not changed, so it doubles
as a redeploy button.

Webhook mode requires a bearer token containing at least 32 printable non-space
ASCII bytes. Prefer the environment so the token is not stored in the config or
placed in process arguments:

```sh
export TINYCD_TOKEN="$(openssl rand -hex 32)"
tinycd

curl -X POST \
  -H "Authorization: Bearer $TINYCD_TOKEN" \
  http://localhost:8080/deploy
```

A token may be placed in `tinycd.toml`, but on Unix the file must not be
readable by group or other users. Use HTTPS through a reverse proxy whenever
the webhook crosses an untrusted network. Tokens and repository URLs supplied
to the sync command are removed from install and start environments.

## Deployments

The default layout is:

```text
.tinycd/
├── current -> deployments/1751392800000
├── head
└── deployments/
    ├── 1751392700000/
    └── 1751392800000/
```

The sync command starts in a new empty deployment folder. `TINYCD_REPO` is
available only to that command. Install and start run in the populated folder.
Failed syncs and installs are removed, and folders left behind by interrupted
deployments are cleaned up on the next deployment. Successful releases beyond
`keep` are deleted oldest-first after the new release starts. `head` records
the deployed commit so restarts pick up where the last run stopped.

The start command runs in its own process group. When a new release is ready,
or when tinycd itself receives Ctrl+C or SIGTERM, the whole group receives
SIGTERM; anything still running ten seconds later receives SIGKILL.

Create the interlock file to pause before syncing or restarting, then remove it
to continue:

```sh
touch .tinycd/interlock
rm .tinycd/interlock
```

Interlock access errors fail closed.
