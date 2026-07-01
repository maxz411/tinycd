use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use clap::Parser;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// How long a stopping release gets to exit after SIGTERM before SIGKILL.
const GRACE_PERIOD: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Config file. Defaults to tinycd.toml when that file exists.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Poll the remote HEAD every N seconds.
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    poll: Option<u64>,

    /// Listen for POST /deploy requests.
    #[arg(long, value_name = "ADDRESS")]
    hook: Option<SocketAddr>,

    /// Webhook bearer token. Prefer the TINYCD_TOKEN environment variable.
    #[arg(
        long,
        env = "TINYCD_TOKEN",
        value_name = "TOKEN",
        hide_env_values = true
    )]
    token: Option<String>,

    /// Git remote URL or name to poll, such as origin.
    #[arg(long, value_name = "REMOTE")]
    repo: Option<String>,

    /// File that pauses syncs and restarts while it exists.
    #[arg(long, value_name = "PATH")]
    interlock: Option<PathBuf>,

    /// Root containing deployment folders and the current symlink.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,

    /// Command that syncs the local checkout.
    #[arg(long)]
    sync: Option<String>,

    /// Command that installs the project.
    #[arg(long)]
    install: Option<String>,

    /// Command that starts the project.
    #[arg(long)]
    start: Option<String>,

    /// Number of successful deployments to retain.
    #[arg(long)]
    keep: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    poll: Option<u64>,
    hook: Option<SocketAddr>,
    token: Option<String>,
    repo: Option<String>,
    interlock: Option<PathBuf>,
    root: Option<PathBuf>,
    sync: Option<String>,
    install: Option<String>,
    start: Option<String>,
    keep: Option<usize>,
}

impl FileConfig {
    async fn load(path: &Path, required: bool) -> Result<Self, Error> {
        let source = match tokio::fs::read_to_string(path).await {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display()).into()),
        };
        let file: Self = toml::from_str(&source)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;

        #[cfg(unix)]
        if file.token.is_some() {
            use std::os::unix::fs::PermissionsExt;

            let mode = tokio::fs::metadata(path).await?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{} contains a token and must not be readable by group or other users",
                    path.display()
                )
                .into());
            }
        }
        Ok(file)
    }
}

struct Config {
    poll: Option<u64>,
    hook: Option<SocketAddr>,
    /// SHA-256 of the webhook bearer token.
    token: Option<[u8; 32]>,
    repo: String,
    interlock: PathBuf,
    root: PathBuf,
    sync: String,
    install: String,
    start: String,
    keep: usize,
}

impl Config {
    /// Merge the CLI over the config file over the built-in defaults.
    async fn load(cli: Cli) -> Result<Self, Error> {
        let required = cli.config.is_some();
        let path = cli.config.unwrap_or_else(|| PathBuf::from("tinycd.toml"));
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file = FileConfig::load(&path, required).await?;

        let mut poll = cli.poll.or(file.poll);
        let hook = cli.hook.or(file.hook);
        if poll.is_none() && hook.is_none() {
            poll = Some(30);
        }
        if poll == Some(0) {
            return Err("poll must be at least one second".into());
        }

        let token = cli.token.or(file.token);
        if hook.is_some() && token.is_none() {
            return Err("hook mode requires --token, TINYCD_TOKEN, or token in tinycd.toml".into());
        }
        if token.as_ref().is_some_and(|token| {
            token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(
                "the webhook token must be at least 32 printable non-space ASCII bytes".into(),
            );
        }

        // Relative paths in the file are relative to the file; relative CLI
        // paths are relative to the working directory.
        let resolve = |path: PathBuf| {
            if path.is_absolute() {
                path
            } else {
                dir.join(path)
            }
        };
        let root = cli
            .root
            .or_else(|| file.root.map(resolve))
            .unwrap_or_else(|| dir.join(".tinycd"));
        let interlock = cli
            .interlock
            .or_else(|| file.interlock.map(resolve))
            .unwrap_or_else(|| root.join("interlock"));

        let keep = cli.keep.or(file.keep).unwrap_or(5);
        if keep == 0 {
            return Err("keep must be at least one deployment".into());
        }

        Ok(Self {
            poll,
            hook,
            token: token.map(|token| Sha256::digest(token.as_bytes()).into()),
            repo: repo_url(cli.repo.or(file.repo), dir).await?,
            interlock,
            root,
            sync: cli
                .sync
                .or(file.sync)
                .unwrap_or_else(|| "git clone --depth 1 \"$TINYCD_REPO\" .".to_owned()),
            install: cli
                .install
                .or(file.install)
                .unwrap_or_else(|| "true".to_owned()),
            start: cli
                .start
                .or(file.start)
                .ok_or("start must be configured with --start or in tinycd.toml")?,
            keep,
        })
    }
}

/// Expand a remote name (or the default, origin) into its URL via the Git
/// project next to the config file. Values that are not a known remote are
/// used as-is.
async fn repo_url(configured: Option<String>, dir: &Path) -> Result<String, Error> {
    let remote = configured.as_deref().unwrap_or("origin");
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(dir)
        .env_remove("TINYCD_TOKEN")
        .env_remove("TINYCD_REPO")
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        _ => Ok(configured
            .ok_or("repo is not configured and the current project has no origin remote")?),
    }
}

#[derive(Default)]
struct Running {
    child: Option<Child>,
    /// Remote HEAD at the last successful deployment.
    head: Option<String>,
}

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    running: Arc<Mutex<Running>>,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tinycd: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Error> {
    let app = App {
        config: Arc::new(Config::load(Cli::parse()).await?),
        running: Arc::default(),
    };

    resume(&app).await?;

    let run = async {
        match (app.config.poll, app.config.hook) {
            (Some(seconds), None) => poll(app.clone(), seconds).await,
            (None, Some(address)) => serve(app.clone(), address).await,
            (Some(seconds), Some(address)) => {
                tokio::try_join!(poll(app.clone(), seconds), serve(app.clone(), address))?;
                Ok(())
            }
            (None, None) => unreachable!("Config::load defaults to polling"),
        }
    };

    tokio::select! {
        result = run => result,
        _ = shutdown_signal() => {
            println!("shutting down");
            if let Some(child) = app.running.lock().await.child.take() {
                stop_server(child).await?;
            }
            Ok(())
        }
    }
}

/// Wait for Ctrl+C or, on Unix, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Restart the release that current points at, if a previous run left one.
async fn resume(app: &App) -> Result<(), Error> {
    let config = &app.config;
    let current = match tokio::fs::canonicalize(config.root.join("current")).await {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let deployments = tokio::fs::canonicalize(config.root.join("deployments")).await?;
    if !current.starts_with(deployments) {
        return Err("current points outside the deployments directory".into());
    }

    wait_for_interlock(&config.interlock).await?;
    let head = tokio::fs::read_to_string(config.root.join("head"))
        .await
        .ok()
        .map(|head| head.trim().to_owned())
        .filter(|head| !head.is_empty());
    println!("running start: {}", config.start);
    *app.running.lock().await = Running {
        child: Some(start_server(config, &current)?),
        head,
    };
    Ok(())
}

async fn poll(app: App, seconds: u64) -> Result<(), Error> {
    loop {
        match remote_head(&app.config.repo).await {
            Ok(next) => {
                let head = app.running.lock().await.head.clone();
                let outdated = match &head {
                    Some(head) => *head != next,
                    // Nothing recorded: deploy unless a release predating the
                    // head file is already running.
                    None => tokio::fs::metadata(app.config.root.join("current"))
                        .await
                        .is_err(),
                };
                if outdated {
                    if let Err(error) = deploy(&app).await {
                        eprintln!("deployment failed: {error}");
                    }
                } else if head.is_none() {
                    app.running.lock().await.head = Some(next);
                }
            }
            Err(error) => eprintln!("failed to poll {}: {error}", app.config.repo),
        }

        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }
}

async fn remote_head(repo: &str) -> Result<String, Error> {
    let output = Command::new("git")
        .args(["ls-remote", repo, "HEAD"])
        .env_remove("TINYCD_TOKEN")
        .env_remove("TINYCD_REPO")
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git ls-remote exited with {}: {}",
            output.status,
            stderr.trim()
        )
        .into());
    }

    match String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
    {
        Some(head) => Ok(head.to_owned()),
        None => Err(format!("{repo} has no HEAD").into()),
    }
}

async fn serve(app: App, address: SocketAddr) -> Result<(), Error> {
    let router = Router::new()
        .route(
            "/deploy",
            post(|State(app): State<App>, headers: HeaderMap| async move {
                if !authorized(&headers, app.config.token.as_ref()) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        [(header::WWW_AUTHENTICATE, "Bearer")],
                        "unauthorized",
                    )
                        .into_response();
                }

                match deploy(&app).await {
                    Ok(()) => (StatusCode::OK, "deployed").into_response(),
                    Err(error) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
                    }
                }
            }),
        )
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(address).await?;

    println!("listening for POST /deploy on {address}");
    axum::serve(listener, router).await?;
    Ok(())
}

fn authorized(headers: &HeaderMap, expected: Option<&[u8; 32]>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, token)| {
            let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            scheme.eq_ignore_ascii_case("bearer") && bool::from(digest.ct_eq(expected))
        })
}

async fn deploy(app: &App) -> Result<(), Error> {
    let config = &app.config;
    let mut running = app.running.lock().await;

    wait_for_interlock(&config.interlock).await?;
    let head = remote_head(&config.repo).await?;

    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string();
    let deployments = config.root.join("deployments");
    let staging = deployments.join(format!(".{id}"));
    let release = deployments.join(&id);
    tokio::fs::create_dir_all(&deployments).await?;
    tokio::fs::create_dir(&staging).await?;

    if let Err(error) = build(config, &staging, &release).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error);
    }

    if let Some(child) = running.child.take() {
        stop_server(child).await?;
    }
    println!("running start: {}", config.start);
    running.child = Some(start_server(config, &release)?);

    if let Err(error) = point_current(&config.root, &id) {
        if let Some(child) = running.child.take() {
            let _ = stop_server(child).await;
        }
        return Err(format!("failed to update the current deployment: {error}").into());
    }
    if let Err(error) = tokio::fs::write(config.root.join("head"), format!("{head}\n")).await {
        eprintln!("failed to record the deployed commit: {error}");
    }
    running.head = Some(head);

    prune(&deployments, &release, config.keep).await;
    println!("deployed {}", release.display());
    Ok(())
}

/// Run the sync and install commands in the staging folder, then promote it to
/// a release folder once the interlock allows.
async fn build(config: &Config, staging: &Path, release: &Path) -> Result<(), Error> {
    for (name, command) in [("sync", &config.sync), ("install", &config.install)] {
        println!("running {name}: {command}");
        let mut process = Command::new("sh");
        process
            .args(["-c", command])
            .current_dir(staging)
            .env_remove("TINYCD_TOKEN")
            .env_remove("TINYCD_REPO");
        if name == "sync" {
            process.env("TINYCD_REPO", &config.repo);
        }

        let status = process.status().await?;
        if !status.success() {
            return Err(format!("{name} command exited with {status}").into());
        }
    }

    // The interlock may have appeared during a slow install; recheck before
    // touching the running server.
    wait_for_interlock(&config.interlock).await?;
    tokio::fs::rename(staging, release).await?;
    Ok(())
}

fn start_server(config: &Config, release: &Path) -> Result<Child, Error> {
    let mut command = Command::new("sh");
    command
        .args(["-c", &config.start])
        .current_dir(release)
        .env_remove("TINYCD_TOKEN")
        .env_remove("TINYCD_REPO")
        .kill_on_drop(true);
    // A dedicated process group lets stop_server signal every process the
    // start command spawns, not just the shell.
    #[cfg(unix)]
    command.process_group(0);
    Ok(command.spawn()?)
}

/// Stop a release: SIGTERM its process group, wait up to GRACE_PERIOD, then
/// SIGKILL whatever remains. Windows falls back to killing the shell.
async fn stop_server(mut child: Child) -> Result<(), Error> {
    #[cfg(unix)]
    if let Some(group) = child.id() {
        unsafe { libc::killpg(group as i32, libc::SIGTERM) };
        if tokio::time::timeout(GRACE_PERIOD, child.wait())
            .await
            .is_err()
        {
            eprintln!(
                "server did not exit within {} seconds, killing it",
                GRACE_PERIOD.as_secs()
            );
        }
        unsafe { libc::killpg(group as i32, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = child.kill().await;

    child
        .wait()
        .await
        .map_err(|error| format!("failed to stop the running server: {error}"))?;
    Ok(())
}

/// Atomically repoint the current symlink at the release with this id.
fn point_current(root: &Path, id: &str) -> std::io::Result<()> {
    let staging = root.join(format!(".current-{id}"));
    let target = Path::new("deployments").join(id);
    #[cfg(unix)]
    let link = std::os::unix::fs::symlink(target, &staging);
    #[cfg(windows)]
    let link = std::os::windows::fs::symlink_dir(target, &staging);

    link.and_then(|()| std::fs::rename(&staging, root.join("current")))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&staging);
        })
}

/// Delete releases beyond the retention limit and staging folders left behind
/// by interrupted deployments. Failures only log: the new release is live.
async fn prune(deployments: &Path, release: &Path, keep: usize) {
    let Ok(mut entries) = tokio::fs::read_dir(deployments).await else {
        return;
    };
    let mut releases = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path == release || !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            remove(&path).await;
        } else {
            releases.push(path);
        }
    }

    releases.sort();
    for old in releases
        .iter()
        .take(releases.len().saturating_sub(keep - 1))
    {
        remove(old).await;
    }
}

async fn remove(path: &Path) {
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        eprintln!("failed to remove {}: {error}", path.display());
    }
}

async fn wait_for_interlock(path: &Path) -> Result<(), Error> {
    let mut waiting = false;

    loop {
        match tokio::fs::symlink_metadata(path).await {
            Ok(_) => {
                if !waiting {
                    println!("waiting for interlock to disappear: {}", path.display());
                    waiting = true;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if waiting {
                    println!("interlock removed: {}", path.display());
                }
                return Ok(());
            }
            Err(error) => {
                return Err(
                    format!("failed to check interlock {}: {error}", path.display()).into(),
                );
            }
        }
    }
}
