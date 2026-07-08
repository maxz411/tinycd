mod setup;

use std::{
    collections::BTreeMap,
    io::IsTerminal,
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

/// How long a stopping release gets to exit gracefully before it is killed.
const GRACE_PERIOD: Duration = Duration::from_secs(10);

/// How long a started server must stay up to count as running.
const DRY_RUN_WINDOW: Duration = Duration::from_secs(3);

/// How long a configured check command gets to pass, retrying every second.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "\
Examples:
  tinycd                                       track this checkout's remote
  tinycd ~/code/app                            track another checkout
  tinycd git@github.com:example/app.git        deploy a URL into this directory
  tinycd https://github.com/e/app.git ~/blog   deploy a URL into ~/blog"
)]
struct Cli {
    /// Git URL to deploy, or a Git checkout to track. Defaults to the
    /// current directory.
    #[arg(value_name = "SOURCE")]
    source: Option<String>,

    /// Directory that holds the deployments when SOURCE is a Git URL.
    /// Defaults to the current directory.
    #[arg(value_name = "DIR")]
    dir: Option<PathBuf>,

    /// Config file. Defaults to .tinycd/config.toml in the tracked directory.
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

    /// Branch or tag to poll and clone instead of the remote HEAD.
    #[arg(long = "ref", value_name = "REF")]
    git_ref: Option<String>,

    /// Path linked into each release from <root>/shared; repeatable. A
    /// trailing slash marks a directory that does not exist yet.
    #[arg(long, value_name = "PATH")]
    share: Vec<String>,

    /// Command that must exit 0 before a started release replaces current.
    #[arg(long)]
    check: Option<String>,

    /// File that pauses syncs and restarts while it exists.
    #[arg(long, value_name = "PATH")]
    interlock: Option<PathBuf>,

    /// Root containing deployment folders and the current symlink.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,

    /// Shell that runs sync, install, and start; repeat per argument, such as
    /// --shell powershell --shell -Command. Defaults to sh -c (cmd /C on Windows).
    #[arg(long, value_name = "ARG", allow_hyphen_values = true)]
    shell: Vec<String>,

    /// Extra KEY=VALUE environment for sync, install, and start; repeatable.
    #[arg(long, value_name = "KEY=VALUE")]
    env: Vec<String>,

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

    /// Sync, install, and start one release as a test, verify it stays up
    /// briefly, then stop it and exit without touching the deployed state.
    #[arg(long, conflicts_with_all = ["status", "rollback"])]
    dry_run: bool,

    /// Show the deployed state and exit.
    #[arg(long, conflicts_with = "rollback")]
    status: bool,

    /// Repoint current at an earlier release and exit; stop tinycd first.
    /// Without a value, the release before the current one is used.
    #[arg(
        long,
        value_name = "ID",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    rollback: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    poll: Option<u64>,
    hook: Option<SocketAddr>,
    token: Option<String>,
    repo: Option<String>,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    share: Option<Vec<String>>,
    interlock: Option<PathBuf>,
    root: Option<PathBuf>,
    shell: Option<Vec<String>>,
    env: BTreeMap<String, String>,
    sync: Option<String>,
    install: Option<String>,
    start: Option<String>,
    check: Option<String>,
    keep: Option<usize>,
}

/// Setting names that signal a misplaced key inside the [env] table.
const SETTING_NAMES: [&str; 15] = [
    "poll",
    "hook",
    "token",
    "repo",
    "ref",
    "share",
    "interlock",
    "root",
    "shell",
    "sync",
    "install",
    "start",
    "check",
    "keep",
    "env",
];

impl FileConfig {
    async fn load(path: &Path, required: bool) -> Result<Self, Error> {
        let source = match tokio::fs::read_to_string(path).await {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display()).into()),
        };
        toml::from_str(&source)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()).into())
    }
}

struct Config {
    poll: Option<u64>,
    hook: Option<SocketAddr>,
    /// The webhook secret, kept raw because the GitHub scheme signs the
    /// request body with it.
    token: Option<String>,
    repo: String,
    /// Branch or tag to poll and clone; the remote HEAD when unset.
    git_ref: Option<String>,
    /// Paths linked into each release from <root>/shared after sync.
    share: Vec<String>,
    interlock: PathBuf,
    root: PathBuf,
    sync: String,
    keep: usize,
    /// Project settings from the CLI and the local config file. They override
    /// the tinycd.toml inside the repository, which is merged per release.
    overrides: Overrides,
}

/// The settings a repository's own tinycd.toml may provide.
struct Overrides {
    shell: Option<Vec<String>>,
    env: BTreeMap<String, String>,
    install: Option<String>,
    start: Option<String>,
    check: Option<String>,
}

impl Overrides {
    /// Resolve the shell: the overrides win, then a repository's tinycd.toml,
    /// then the platform default.
    fn resolve_shell(&self, file: Option<Vec<String>>) -> Vec<String> {
        self.shell.clone().or(file).unwrap_or_else(default_shell)
    }
}

impl Config {
    /// Merge the CLI over the local config file over the built-in defaults.
    /// Settings that describe the project itself may also come from the
    /// .tinycd/config.toml inside the repository; those are merged per
    /// release.
    async fn load(cli: Cli, mode: Mode) -> Result<Self, Error> {
        if cli.repo.is_some() && matches!(mode, Mode::Url { .. }) {
            return Err(
                "pass the repository as either the first argument or --repo, not both".into(),
            );
        }
        if let (Mode::Url { repo, home }, None) = (&mode, &cli.config) {
            write_local_config(home, repo)?;
        }
        let project = match &mode {
            Mode::Local { project } => project.clone(),
            Mode::Url { home, .. } => home.clone(),
        };

        let required = cli.config.is_some();
        let path = cli
            .config
            .unwrap_or_else(|| project.join(".tinycd").join("config.toml"));
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file = FileConfig::load(&path, required).await?;

        // Only the local file's token is used, so only it must be private.
        #[cfg(unix)]
        if file.token.is_some() {
            use std::os::unix::fs::PermissionsExt;

            let mode = tokio::fs::metadata(&path).await?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{} contains a token and must not be readable by group or other users",
                    path.display()
                )
                .into());
            }
        }

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
            return Err(
                "hook mode requires --token, TINYCD_TOKEN, or token in .tinycd/config.toml".into(),
            );
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
            let path = if path.is_absolute() {
                path
            } else {
                dir.join(path)
            };
            std::path::absolute(&path).unwrap_or(path)
        };
        let root = match (cli.root, file.root) {
            (Some(root), _) => root,
            (None, Some(root)) => resolve(root),
            (None, None) => match &mode {
                Mode::Local { project } => state_root(project)?,
                Mode::Url { home, .. } => home.join(".tinycd"),
            },
        };
        let interlock = cli
            .interlock
            .or_else(|| file.interlock.map(resolve))
            .unwrap_or_else(|| root.join("interlock"));

        let shell = if cli.shell.is_empty() {
            file.shell
        } else {
            Some(cli.shell)
        };
        if shell.as_deref().is_some_and(|shell| shell.is_empty()) {
            return Err("shell must name a program".into());
        }

        let mut env = file.env;
        // A tinycd setting inside [env] is almost always a misplaced key:
        // everything below the table header lands in the table.
        for key in env.keys() {
            if SETTING_NAMES.contains(&key.as_str()) {
                return Err(format!(
                    "[env] contains '{key}', which is a tinycd setting; move it above the \
                     [env] table, or pass --env {key}=... for a variable really named that"
                )
                .into());
            }
        }
        for pair in cli.env {
            match pair.split_once('=') {
                Some((key, value)) if !key.is_empty() => {
                    env.insert(key.to_owned(), value.to_owned());
                }
                _ => return Err(format!("--env {pair} must look like KEY=VALUE").into()),
            }
        }

        let git_ref = cli.git_ref.or(file.git_ref);
        let default_sync = match (cfg!(windows), git_ref.is_some()) {
            (false, false) => r#"git clone --depth 1 "$TINYCD_REPO" ."#,
            (false, true) => r#"git clone --depth 1 --branch "$TINYCD_REF" "$TINYCD_REPO" ."#,
            (true, false) => r#"git clone --depth 1 "%TINYCD_REPO%" ."#,
            (true, true) => r#"git clone --depth 1 --branch "%TINYCD_REF%" "%TINYCD_REPO%" ."#,
        };

        let keep = cli.keep.or(file.keep).unwrap_or(5);
        if keep == 0 {
            return Err("keep must be at least one deployment".into());
        }

        let repo = match mode {
            Mode::Url { repo, .. } => repo,
            Mode::Local { .. } => repo_url(cli.repo.or(file.repo), &project).await?,
        };

        let mut share = if cli.share.is_empty() {
            file.share.unwrap_or_default()
        } else {
            cli.share
        };
        for entry in &mut share {
            let trimmed = entry.trim_end_matches(['/', '\\']);
            if trimmed.is_empty()
                || Path::new(trimmed)
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(format!("share entry {entry:?} must be a relative path").into());
            }
        }

        Ok(Self {
            poll,
            hook,
            token,
            repo,
            git_ref,
            share,
            interlock,
            root,
            sync: cli
                .sync
                .or(file.sync)
                .unwrap_or_else(|| default_sync.to_owned()),
            keep,
            overrides: Overrides {
                shell,
                env,
                install: cli.install.or(file.install),
                start: cli.start.or(file.start),
                check: cli.check.or(file.check),
            },
        })
    }
}

fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".to_owned(), "/C".to_owned()]
    } else {
        vec!["sh".to_owned(), "-c".to_owned()]
    }
}

/// Where the repository and the deployment state come from.
enum Mode {
    /// Track a local checkout: poll its remote and keep deployments under
    /// ~/.tinycd so the working tree stays clean.
    Local { project: PathBuf },
    /// Deploy a Git URL: configuration and deployments live in home.
    Url { repo: String, home: PathBuf },
}

impl Mode {
    fn resolve(source: Option<String>, dir: Option<PathBuf>) -> Result<Self, Error> {
        let Some(source) = source else {
            return Ok(Self::Local {
                project: project_dir(Path::new("."))?,
            });
        };
        if looks_like_git_url(&source) {
            let home = dir.unwrap_or_else(|| PathBuf::from("."));
            std::fs::create_dir_all(&home)
                .map_err(|error| format!("failed to create {}: {error}", home.display()))?;
            return Ok(Self::Url {
                repo: source,
                home: std::path::absolute(&home)?,
            });
        }
        if dir.is_some() {
            return Err("a second path is only accepted after a Git URL".into());
        }
        Ok(Self::Local {
            project: project_dir(Path::new(&source))?,
        })
    }
}

fn project_dir(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_dir() {
        return Err(format!(
            "{} is neither an existing directory nor a Git URL",
            path.display()
        )
        .into());
    }
    Ok(std::path::absolute(path)?)
}

/// A scheme:// URL or an scp-like [user@]host:path. Windows drive paths such
/// as C:\app have a single-letter host and stay paths.
fn looks_like_git_url(source: &str) -> bool {
    source.contains("://")
        || source
            .split_once(':')
            .is_some_and(|(host, _)| host.len() > 1 && !host.contains('/') && !host.contains('\\'))
}

/// Deployments for a tracked checkout live under ~/.tinycd, keyed by the
/// checkout's absolute path, so the working tree stays clean and a restart
/// finds the same releases.
fn state_root(project: &Path) -> Result<PathBuf, Error> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(variable)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{variable} must be set to place deployments; or pass --root"))?;

    let name: String = project
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_owned())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let digest = Sha256::digest(project.as_os_str().as_encoded_bytes());
    let key = format!(
        "{name}-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    );
    Ok(home.join(".tinycd").join(key))
}

/// Record the deployed repository next to the deployments so a plain `tinycd`
/// run there restarts the same deployment. Files tinycd wrote earlier are
/// refreshed when the URL changes; files the user authored are left alone.
fn write_local_config(home: &Path, repo: &str) -> Result<(), Error> {
    const MARKER: &str = "# Written by tinycd";

    let dir = home.join(".tinycd");
    let path = dir.join("config.toml");
    let repo = repo.replace('\\', r"\\").replace('"', "\\\"");
    let contents = format!(
        "{MARKER}; running `tinycd` in this directory resumes the deployment.\n\
         repo = \"{repo}\"\nroot = \".\"\n"
    );
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == contents || !existing.starts_with(MARKER) => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("failed to read {}: {error}", path.display()).into()),
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    std::fs::write(&path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Hold an exclusive lock on <root>/lock so a second tinycd cannot manage the
/// same root. Roots never overlap between projects, so each project gets its
/// own instance. The lock is advisory and dies with the process.
fn lock_root(root: &Path) -> Result<std::fs::File, Error> {
    std::fs::create_dir_all(root)
        .map_err(|error| format!("failed to create {}: {error}", root.display()))?;
    let path = root.join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err(format!("another tinycd is already managing {}", root.display()).into())
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(format!("failed to lock {}: {error}", path.display()).into())
        }
    }
}

/// A git invocation that never sees tinycd's own secrets.
fn git(args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(args)
        .env_remove("TINYCD_TOKEN")
        .env_remove("TINYCD_REPO")
        .env_remove("TINYCD_REF");
    command
}

/// Expand a remote name (or the default, origin) into its URL via the Git
/// project being tracked. Values that are not a known remote are used as-is.
async fn repo_url(configured: Option<String>, dir: &Path) -> Result<String, Error> {
    let remote = configured.as_deref().unwrap_or("origin");
    let output = git(&["remote", "get-url", remote])
        .current_dir(dir)
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        _ => Ok(configured.ok_or_else(|| {
            format!(
                "{} is not a Git checkout with an origin remote; pass a Git URL or set repo in .tinycd/config.toml",
                dir.display()
            )
        })?),
    }
}

#[derive(Default)]
struct Running {
    server: Option<Server>,
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
    let mut cli = Cli::parse();
    let mode = Mode::resolve(cli.source.take(), cli.dir.take())?;

    // First run in an unconfigured checkout on an interactive terminal:
    // guide the user through setup instead of failing on a missing start.
    if let Mode::Local { project } = &mode
        && cli.config.is_none()
        && cli.start.is_none()
        && !cli.status
        && cli.rollback.is_none()
        && !project.join(".tinycd").join("config.toml").exists()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
        let origin = repo_url(None, project).await.ok();
        let Some(answers) = setup::run(project, origin.as_deref())? else {
            println!("setup cancelled; run tinycd here again to retry");
            return Ok(());
        };
        let path = setup::write_config(project, &answers)?;
        println!("wrote {}", path.display());
    }

    let show_status = cli.status;
    let rollback_to = cli.rollback.clone();
    let dry = cli.dry_run;
    let config = Config::load(cli, mode).await?;
    if show_status {
        return status(&config).await;
    }
    if let Some(target) = rollback_to {
        return rollback(&config, &target).await;
    }
    if dry {
        return dry_run(&config).await;
    }

    let app = App {
        config: Arc::new(config),
        running: Arc::default(),
    };
    let _lock = lock_root(&app.config.root)?;
    println!("tracking {}", app.config.repo);
    println!("deployments in {}", app.config.root.display());

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
            if let Some(server) = app.running.lock().await.server.take() {
                server.stop().await?;
            }
            Ok(())
        }
    }
}

/// Wait for Ctrl+C, SIGTERM or SIGHUP on Unix, or a console close or system
/// shutdown on Windows. SIGHUP arrives when the terminal that started tinycd
/// closes; without stopping, the release would outlive tinycd unmanaged.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term =
            signal(SignalKind::terminate()).expect("failed to install the SIGTERM handler");
        let mut hangup =
            signal(SignalKind::hangup()).expect("failed to install the SIGHUP handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hangup.recv() => {}
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows;

        let mut ctrl_break =
            windows::ctrl_break().expect("failed to install the Ctrl+Break handler");
        let mut ctrl_close = windows::ctrl_close().expect("failed to install the close handler");
        let mut ctrl_shutdown =
            windows::ctrl_shutdown().expect("failed to install the shutdown handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = ctrl_break.recv() => {}
            _ = ctrl_close.recv() => {}
            _ = ctrl_shutdown.recv() => {}
        }
    }

    #[cfg(not(any(unix, windows)))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Restart the release that current points at, if a previous run left one.
async fn resume(app: &App) -> Result<(), Error> {
    let config = &app.config;
    let current = config.root.join("current");
    match tokio::fs::canonicalize(&current).await {
        Ok(resolved) => {
            let deployments = tokio::fs::canonicalize(config.root.join("deployments")).await?;
            if !resolved.starts_with(deployments) {
                return Err("current points outside the deployments directory".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }

    wait_for_interlock(&config.interlock).await?;
    let head = tokio::fs::read_to_string(config.root.join("head"))
        .await
        .ok()
        .map(|head| head.trim().to_owned())
        .filter(|head| !head.is_empty());
    let spec = run_spec(config, &current).await?;
    let resolved = tokio::fs::canonicalize(&current).await?;
    let id = resolved.file_name().unwrap_or_default().to_string_lossy();
    let log = open_log(&release_log(&config.root, &id)).await;
    banner(log.as_ref(), &format!("running start: {}", spec.start));
    *app.running.lock().await = Running {
        server: Some(start_server(&spec, &current, log.as_ref())?),
        head,
    };
    Ok(())
}

async fn poll(app: App, seconds: u64) -> Result<(), Error> {
    loop {
        match remote_head(&app.config.repo, app.config.git_ref.as_deref()).await {
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

/// The commit the configured ref (or HEAD) points at on the remote. Branches
/// sort before tags in ls-remote output, so a name naming both picks the
/// branch.
async fn remote_head(repo: &str, git_ref: Option<&str>) -> Result<String, Error> {
    let target = git_ref.unwrap_or("HEAD");
    let output = git(&["ls-remote", repo, target]).output().await?;
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
        None => Err(format!("{repo} has no ref {target}").into()),
    }
}

async fn serve(app: App, address: SocketAddr) -> Result<(), Error> {
    let router = Router::new()
        .route(
            "/deploy",
            post(
                |State(app): State<App>, headers: HeaderMap, body: axum::body::Bytes| async move {
                    if !authorized(&headers, &body, app.config.token.as_deref()) {
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
                },
            ),
        )
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("failed to listen on {address}: {error}"))?;

    println!("listening for POST /deploy on {address}");
    axum::serve(listener, router).await?;
    Ok(())
}

/// Accept the token however the sender can deliver it: a bearer header, a
/// GitLab webhook's X-Gitlab-Token, or a GitHub webhook's HMAC signature of
/// the body. Comparisons run over digests in constant time.
fn authorized(headers: &HeaderMap, body: &[u8], token: Option<&str>) -> bool {
    let Some(token) = token else {
        return false;
    };
    let expected: [u8; 32] = Sha256::digest(token.as_bytes()).into();

    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        && let Some((scheme, presented)) = value.split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        if bool::from(digest.ct_eq(&expected)) {
            return true;
        }
    }

    if let Some(presented) = headers
        .get("x-gitlab-token")
        .and_then(|value| value.to_str().ok())
    {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        if bool::from(digest.ct_eq(&expected)) {
            return true;
        }
    }

    if let Some(value) = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        && let Some(hex) = value.strip_prefix("sha256=")
        && let Some(signature) = decode_hex32(hex)
    {
        let computed = hmac_sha256(token.as_bytes(), body);
        if bool::from(signature.ct_eq(&computed)) {
            return true;
        }
    }

    false
}

/// HMAC-SHA256 (RFC 2104) over the sha2 crate; the textbook two-pass
/// construction, kept inline to avoid a dependency.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn decode_hex32(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.as_bytes();
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, pair) in hex.chunks_exact(2).enumerate() {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        bytes[index] = (high * 16 + low) as u8;
    }
    Some(bytes)
}

async fn deploy(app: &App) -> Result<(), Error> {
    let config = &app.config;
    let mut running = app.running.lock().await;

    wait_for_interlock(&config.interlock).await?;
    let head = remote_head(&config.repo, config.git_ref.as_deref()).await?;

    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string();
    let deployments = config.root.join("deployments");
    let release = deployments.join(&id);
    // Releases are built under their final name: renaming a folder after
    // install would break the absolute paths that Python and Node tooling
    // bake into virtualenvs and shebangs. The marker distinguishes a
    // completed release from one interrupted mid-build.
    let marker = deployments.join(format!(".incomplete-{id}"));
    tokio::fs::create_dir_all(&deployments).await?;
    tokio::fs::write(&marker, "").await?;
    tokio::fs::create_dir(&release).await?;
    let log_path = release_log(&config.root, &id);
    let log = open_log(&log_path).await;
    let cite = |error: Error| format!("{error} (log: {})", log_path.display());

    let spec = match build(config, &release, log.as_ref()).await {
        Ok(spec) => spec,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&release).await;
            let _ = tokio::fs::remove_file(&marker).await;
            return Err(cite(error).into());
        }
    };

    // The interlock may have appeared during a slow install; recheck before
    // touching the running server.
    wait_for_interlock(&config.interlock).await?;
    tokio::fs::remove_file(&marker).await?;

    if let Some(server) = running.server.take() {
        server.stop().await?;
    }
    banner(log.as_ref(), &format!("running start: {}", spec.start));
    let server = start_server(&spec, &release, log.as_ref())?;
    match verify_start(server, &spec, &release, log.as_ref()).await {
        Ok(server) => running.server = Some(server),
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&release).await;
            restart_previous(config, &mut running).await;
            return Err(cite(error).into());
        }
    }

    if let Err(error) = point_current(&config.root, &id) {
        if let Some(server) = running.server.take() {
            let _ = server.stop().await;
        }
        return Err(format!("failed to update the current deployment: {error}").into());
    }
    if let Err(error) = tokio::fs::write(config.root.join("head"), format!("{head}\n")).await {
        eprintln!("failed to record the deployed commit: {error}");
    }
    running.head = Some(head);

    prune(&deployments, &release, config.keep).await;
    prune_logs(&config.root.join("logs"), config.keep).await;
    println!("deployed {}", release.display());
    Ok(())
}

/// A started release must stay up for DRY_RUN_WINDOW, and pass the check
/// command within CHECK_TIMEOUT when one is configured. Returns the server
/// still running; on failure nothing is left running.
async fn verify_start(
    mut server: Server,
    spec: &RunSpec,
    release: &Path,
    log: Option<&std::fs::File>,
) -> Result<Server, Error> {
    if let Ok(status) = tokio::time::timeout(DRY_RUN_WINDOW, server.child.wait()).await {
        return Err(format!(
            "the server exited with {} within {} seconds of starting",
            status?,
            DRY_RUN_WINDOW.as_secs()
        )
        .into());
    }

    if let Some(check) = &spec.check {
        banner(log, &format!("running check: {check}"));
        let deadline = std::time::Instant::now() + CHECK_TIMEOUT;
        loop {
            if let Some(status) = server.child.try_wait()? {
                return Err(format!("the server exited with {status} during the check").into());
            }
            let mut process = shell_command(&spec.shell, &spec.env, check, release);
            redirect(&mut process, log)?;
            if process.status().await?.success() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = server.stop().await;
                return Err(format!(
                    "the check command did not pass within {} seconds",
                    CHECK_TIMEOUT.as_secs()
                )
                .into());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    Ok(server)
}

/// Build and start one throwaway release under <root>/.dry-run-<id>, verify
/// it the same way a deployment would, then stop it. The deployed state —
/// current, head, releases, the lock — is never touched, so a dry run is
/// safe next to a live instance (though the started server may contend for
/// the same port).
async fn dry_run(config: &Config) -> Result<(), Error> {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .to_string();
    let scratch = config.root.join(format!(".dry-run-{id}"));
    let release = scratch.join("release");
    tokio::fs::create_dir_all(&release).await?;

    let result = async {
        let spec = build(config, &release, None).await?;
        banner(None, &format!("running start: {}", spec.start));
        let server = start_server(&spec, &release, None)?;
        let server = verify_start(server, &spec, &release, None).await?;
        server.stop().await?;
        Ok::<(), Error>(())
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&scratch).await;
    result?;
    println!(
        "dry run passed: the release built, started, stayed up for {} seconds, and stopped cleanly",
        DRY_RUN_WINDOW.as_secs()
    );
    Ok(())
}

/// Print the deployed state as recorded on disk; works while a daemon runs.
async fn status(config: &Config) -> Result<(), Error> {
    println!("repo     {}", config.repo);
    if let Some(git_ref) = &config.git_ref {
        println!("ref      {git_ref}");
    }
    println!("root     {}", config.root.display());

    match tokio::fs::canonicalize(config.root.join("current")).await {
        Ok(resolved) => {
            let id = resolved
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let age = id
                .parse::<u64>()
                .ok()
                .and_then(|millis| {
                    let deployed = UNIX_EPOCH + Duration::from_millis(millis);
                    SystemTime::now().duration_since(deployed).ok()
                })
                .map(|age| format!(" (deployed {} ago)", format_age(age)))
                .unwrap_or_default();
            println!("current  {id}{age}");
            if let Ok(head) = tokio::fs::read_to_string(config.root.join("head")).await {
                println!("commit   {}", head.trim());
            }
            let log = release_log(&config.root, &id);
            if tokio::fs::metadata(&log).await.is_ok() {
                println!("log      {}", log.display());
            }
        }
        Err(_) => println!("current  none (never deployed)"),
    }

    let mut releases = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(config.root.join("deployments")).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.is_ok_and(|kind| kind.is_dir())
                && !entry.file_name().to_string_lossy().starts_with('.')
            {
                releases += 1;
            }
        }
    }
    println!("releases {releases} retained");

    // Probing the lock without blocking tells us whether a daemon is live.
    let running = match std::fs::File::open(config.root.join("lock")) {
        Ok(file) => matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        Err(_) => false,
    };
    println!(
        "daemon   {}",
        if running { "running" } else { "not running" }
    );
    Ok(())
}

fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
        _ => format!("{}d {}h", seconds / 86400, (seconds % 86400) / 3600),
    }
}

/// Repoint current at an earlier complete release; the daemon must not be
/// running. head is left alone so the rollback survives until the remote
/// actually changes, and the next tinycd run starts the rolled-back release.
async fn rollback(config: &Config, target: &str) -> Result<(), Error> {
    let _lock =
        lock_root(&config.root).map_err(|error| format!("{error}; stop it before rolling back"))?;

    let deployments = config.root.join("deployments");
    let mut releases = Vec::new();
    let mut entries = tokio::fs::read_dir(&deployments)
        .await
        .map_err(|_| "nothing has been deployed yet")?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let complete = !name.starts_with('.')
            && entry.file_type().await.is_ok_and(|kind| kind.is_dir())
            && tokio::fs::metadata(deployments.join(format!(".incomplete-{name}")))
                .await
                .is_err();
        if complete {
            releases.push(name);
        }
    }
    releases.sort();

    let id = if target.is_empty() {
        let current = tokio::fs::canonicalize(config.root.join("current"))
            .await
            .map_err(|_| "nothing is deployed, so there is nothing to roll back")?;
        let current = current
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        releases
            .iter()
            .rev()
            .find(|id| **id < current)
            .ok_or("no release older than the current one is retained")?
            .clone()
    } else {
        releases
            .iter()
            .find(|id| *id == target)
            .ok_or_else(|| {
                format!(
                    "release {target} is not retained; retained: {}",
                    releases.join(", ")
                )
            })?
            .clone()
    };

    point_current(&config.root, &id)?;
    println!("current -> {id}; run tinycd to start it");
    Ok(())
}

/// Bring the release that current points at back up after a failed deploy.
async fn restart_previous(config: &Config, running: &mut Running) {
    let current = config.root.join("current");
    let Ok(resolved) = tokio::fs::canonicalize(&current).await else {
        return;
    };
    let id = resolved.file_name().unwrap_or_default().to_string_lossy();
    let log = open_log(&release_log(&config.root, &id)).await;
    match run_spec(config, &current).await {
        Ok(spec) => {
            banner(log.as_ref(), "restarting the previous release");
            match start_server(&spec, &current, log.as_ref()) {
                Ok(server) => running.server = Some(server),
                Err(error) => eprintln!("failed to restart the previous release: {error}"),
            }
        }
        Err(error) => eprintln!("failed to restart the previous release: {error}"),
    }
}

/// How to install, start, and check one release: the repository's
/// tinycd.toml provides the values, and the CLI or the local config file
/// override them.
struct RunSpec {
    shell: Vec<String>,
    env: BTreeMap<String, String>,
    install: Option<String>,
    start: String,
    check: Option<String>,
}

/// Merge the local overrides with the .tinycd/config.toml inside a synced
/// release. Launcher-level settings in the repository's file are ignored.
async fn run_spec(config: &Config, release: &Path) -> Result<RunSpec, Error> {
    let file = FileConfig::load(&release.join(".tinycd").join("config.toml"), false).await?;
    let overrides = &config.overrides;

    let shell = overrides.resolve_shell(file.shell);
    if shell.is_empty() {
        return Err("shell must name a program".into());
    }
    let mut env = file.env;
    env.extend(overrides.env.clone());

    Ok(RunSpec {
        shell,
        env,
        install: overrides.install.clone().or(file.install),
        start: overrides.start.clone().or(file.start).ok_or(
            "start is not configured; set it in .tinycd/config.toml (committed or local) or with --start",
        )?,
        check: overrides.check.clone().or(file.check),
    })
}

/// Run the sync and install commands in the release folder, linking shared
/// paths in between. The folder already carries its final name; deploy()
/// tracks completion with a marker file instead of a rename.
async fn build(
    config: &Config,
    release: &Path,
    log: Option<&std::fs::File>,
) -> Result<RunSpec, Error> {
    banner(log, &format!("running sync: {}", config.sync));
    let shell = config.overrides.resolve_shell(None);
    let mut process = shell_command(&shell, &config.overrides.env, &config.sync, release);
    process.env("TINYCD_REPO", &config.repo);
    if let Some(git_ref) = &config.git_ref {
        process.env("TINYCD_REF", git_ref);
    }
    redirect(&mut process, log)?;
    let status = process.status().await?;
    if !status.success() {
        return Err(format!("sync command exited with {status}").into());
    }

    if !config.share.is_empty() {
        link_shared(&config.share, &config.root.join("shared"), release)?;
    }

    // Read the freshly synced repository's config, so install and start
    // follow the deployed commit.
    let spec = run_spec(config, release).await?;
    if let Some(install) = &spec.install {
        banner(log, &format!("running install: {install}"));
        let mut process = shell_command(&spec.shell, &spec.env, install, release);
        redirect(&mut process, log)?;
        let status = process.status().await?;
        if !status.success() {
            return Err(format!("install command exited with {status}").into());
        }
    }
    Ok(spec)
}

/// Symlink each shared entry from <shared> into the release, replacing
/// whatever the sync produced at that path. Missing sources are created:
/// entries with a trailing slash as directories, others as empty files. On
/// Windows directories become junctions and files hard links, neither of
/// which needs administrator rights.
fn link_shared(entries: &[String], shared: &Path, release: &Path) -> Result<(), Error> {
    for entry in entries {
        let relative = entry.trim_end_matches(['/', '\\']);
        let source = shared.join(relative);
        let target = release.join(relative);

        if !source.exists() {
            if entry.ends_with(['/', '\\']) {
                std::fs::create_dir_all(&source)?;
            } else {
                if let Some(parent) = source.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(&source)?;
                println!("created empty shared file {}", source.display());
            }
        }

        if let Ok(existing) = std::fs::symlink_metadata(&target) {
            if existing.is_dir() {
                std::fs::remove_dir_all(&target)?;
            } else {
                std::fs::remove_file(&target)?;
            }
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let source = std::path::absolute(&source)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &target)
            .map_err(|error| format!("failed to link {}: {error}", target.display()))?;
        #[cfg(windows)]
        if source.is_dir() {
            junction::create(&source, &target)
                .map_err(|error| format!("failed to link {}: {error}", target.display()))?;
        } else {
            std::fs::hard_link(&source, &target)
                .map_err(|error| format!("failed to link {}: {error}", target.display()))?;
        }
        #[cfg(not(any(unix, windows)))]
        return Err("share is not supported on this platform".into());
    }
    Ok(())
}

/// The per-release log file; sync, install, check, and start output lands
/// here so failed deployments can be examined later.
fn release_log(root: &Path, id: &str) -> PathBuf {
    root.join("logs").join(format!("{id}.log"))
}

/// Open a release log for appending. Logging is best-effort: on failure the
/// commands inherit tinycd's own stdout as before.
async fn open_log(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match std::fs::File::options()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => Some(file),
        Err(error) => {
            eprintln!("failed to open {}: {error}", path.display());
            None
        }
    }
}

/// Print a pipeline step and record it in the release log.
fn banner(log: Option<&std::fs::File>, line: &str) {
    println!("{line}");
    if let Some(mut log) = log {
        use std::io::Write;
        let _ = writeln!(log, "{line}");
    }
}

/// Send a command's output to the release log instead of tinycd's stdout.
fn redirect(command: &mut Command, log: Option<&std::fs::File>) -> Result<(), Error> {
    if let Some(log) = log {
        command
            .stdout(std::process::Stdio::from(log.try_clone()?))
            .stderr(std::process::Stdio::from(log.try_clone()?));
    }
    Ok(())
}

/// A command line run through the given shell with the given environment.
/// Deployment commands never inherit tinycd's own secrets.
fn shell_command(
    shell: &[String],
    env: &BTreeMap<String, String>,
    line: &str,
    dir: &Path,
) -> Command {
    let (program, args) = shell.split_first().expect("shell is validated");
    let mut command = Command::new(program);
    command
        .args(args)
        .arg(line)
        .current_dir(dir)
        .env_remove("TINYCD_TOKEN")
        .env_remove("TINYCD_REPO")
        .env_remove("TINYCD_REF")
        .envs(env);
    command
}

/// A running release. On Windows a job object tracks every process the start
/// command spawns; closing the handle kills whatever is left in the job, so
/// nothing outlives tinycd even if tinycd crashes.
struct Server {
    child: Child,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

fn start_server(
    spec: &RunSpec,
    release: &Path,
    log: Option<&std::fs::File>,
) -> Result<Server, Error> {
    let mut command = shell_command(&spec.shell, &spec.env, &spec.start, release);
    redirect(&mut command, log)?;
    command.kill_on_drop(true);
    // A dedicated process group lets stop() signal every process the start
    // command spawns, not just the shell, and keeps the terminal's Ctrl+C
    // for tinycd's own shutdown handling.
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);

    let child = command.spawn()?;
    #[cfg(windows)]
    let job = assign_job(&child)?;
    Ok(Server {
        child,
        #[cfg(windows)]
        job,
    })
}

impl Server {
    /// Stop the whole process tree: ask politely, wait up to GRACE_PERIOD,
    /// then kill. Politely means SIGTERM to the process group on Unix and
    /// Ctrl+Break on Windows, which only reaches the server when it shares
    /// tinycd's console.
    async fn stop(mut self) -> Result<(), Error> {
        #[cfg(unix)]
        if let Some(group) = self.child.id() {
            unsafe { libc::killpg(group as i32, libc::SIGTERM) };
            if tokio::time::timeout(GRACE_PERIOD, self.child.wait())
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

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;

            let polite = self.child.id().is_some_and(|group| unsafe {
                GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) != 0
            });
            if polite
                && tokio::time::timeout(GRACE_PERIOD, self.child.wait())
                    .await
                    .is_err()
            {
                eprintln!(
                    "server did not exit within {} seconds, killing it",
                    GRACE_PERIOD.as_secs()
                );
            }
            unsafe { TerminateJobObject(self.job.as_raw_handle() as _, 1) };
        }

        self.child
            .wait()
            .await
            .map_err(|error| format!("failed to stop the running server: {error}"))?;
        Ok(())
    }
}

/// Put the spawned server into a new job object that kills every remaining
/// process in the job when the handle closes.
#[cfg(windows)]
fn assign_job(child: &Child) -> Result<std::os::windows::io::OwnedHandle, Error> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }

    let process = child
        .raw_handle()
        .ok_or("the server exited before it could be tracked")?;
    if unsafe { AssignProcessToJobObject(raw, process) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(job)
}

/// Atomically repoint the current symlink at the release with this id.
#[cfg(not(windows))]
fn point_current(root: &Path, id: &str) -> std::io::Result<()> {
    let staging = root.join(format!(".current-{id}"));
    let target = Path::new("deployments").join(id);
    std::os::unix::fs::symlink(target, &staging)
        .and_then(|()| std::fs::rename(&staging, root.join("current")))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&staging);
        })
}

/// Repoint the current junction at the release with this id. Junctions work
/// without administrator rights or Developer Mode, unlike directory symlinks.
#[cfg(windows)]
fn point_current(root: &Path, id: &str) -> std::io::Result<()> {
    let staging = root.join(format!(".current-{id}"));
    let target = std::path::absolute(root.join("deployments").join(id))?;
    junction::create(&target, &staging)?;

    let current = root.join("current");
    std::fs::rename(&staging, &current)
        .or_else(|_| {
            // Windows cannot rename over an existing directory entry. Drop the
            // old junction (an empty directory) and retry; if tinycd dies
            // between the two steps, the next poll redeploys.
            std::fs::remove_dir(&current)?;
            std::fs::rename(&staging, &current)
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_dir(&staging);
        })
}

/// Delete releases beyond the retention limit, plus folders left behind by
/// interrupted deployments: anything dot-prefixed and anything still carrying
/// an .incomplete-<id> marker. Failures only log: the new release is live.
async fn prune(deployments: &Path, release: &Path, keep: usize) {
    let Ok(mut entries) = tokio::fs::read_dir(deployments).await else {
        return;
    };
    let mut incomplete = Vec::new();
    let mut dirs = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path == release {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().await.is_ok_and(|kind| kind.is_dir());
        match (is_dir, name.starts_with('.')) {
            (true, true) => remove(&path).await,
            (true, false) => dirs.push(path),
            (false, _) => {
                if let Some(id) = name.strip_prefix(".incomplete-") {
                    incomplete.push(id.to_owned());
                    if let Err(error) = tokio::fs::remove_file(&path).await {
                        eprintln!("failed to remove {}: {error}", path.display());
                    }
                }
            }
        }
    }

    let mut releases = Vec::new();
    for dir in dirs {
        let id = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if incomplete.contains(&id) {
            remove(&dir).await;
        } else {
            releases.push(dir);
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

/// Keep only the newest `keep` release logs.
async fn prune_logs(logs: &Path, keep: usize) {
    let Ok(mut entries) = tokio::fs::read_dir(logs).await else {
        return;
    };
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "log") {
            files.push(path);
        }
    }
    files.sort();
    for old in files.iter().take(files.len().saturating_sub(keep)) {
        if let Err(error) = tokio::fs::remove_file(old).await {
            eprintln!("failed to remove {}: {error}", old.display());
        }
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
