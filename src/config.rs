//! The two configuration files tinycd reads, and the classic command line:
//! `tinycd [SOURCE] [DIR]` turns a checkout or a Git URL into a project on
//! the local host.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use clap::Args;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    model::{LOCAL_HOST, Project, Recipe, RuntimeKind},
    util::{self, Error},
};

/// The classic invocation: deploy this checkout, that checkout, or a URL.
#[derive(Args, Debug, Default, Clone)]
pub struct QuickArgs {
    /// Git URL to deploy, or a Git checkout to track. Defaults to the
    /// current directory.
    #[arg(value_name = "SOURCE")]
    pub source: Option<String>,

    /// Directory that holds the deployments when SOURCE is a Git URL.
    /// Defaults to the current directory.
    #[arg(value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Config file. Defaults to .tinycd/config.toml in the tracked directory.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Poll the remote HEAD every N seconds.
    #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    pub poll: Option<u64>,

    /// Accept forge webhooks for this project at /api/hooks/<project> on
    /// the dashboard. The value, once a listen address, is ignored.
    #[arg(long, value_name = "ADDRESS", num_args = 0..=1, default_missing_value = "on")]
    pub hook: Option<String>,

    /// Webhook secret. Prefer the TINYCD_TOKEN environment variable.
    #[arg(
        long,
        env = "TINYCD_TOKEN",
        value_name = "TOKEN",
        hide_env_values = true
    )]
    pub token: Option<String>,

    /// Git remote URL or name to poll, such as origin.
    #[arg(long, value_name = "REMOTE")]
    pub repo: Option<String>,

    /// Branch or tag to poll and clone instead of the remote HEAD.
    #[arg(long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,

    /// Path linked into each release from <root>/shared; repeatable. A
    /// trailing slash marks a directory that does not exist yet.
    #[arg(long, value_name = "PATH")]
    pub share: Vec<String>,

    /// Command that must exit 0 before a started release replaces current.
    #[arg(long)]
    pub check: Option<String>,

    /// File that pauses syncs and restarts while it exists.
    #[arg(long, value_name = "PATH")]
    pub interlock: Option<PathBuf>,

    /// Root containing deployment folders and the current symlink.
    #[arg(long, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Shell that runs sync, install, and start; repeat per argument, such as
    /// --shell powershell --shell -Command. Defaults to sh -c (cmd /C on Windows).
    #[arg(long, value_name = "ARG", allow_hyphen_values = true)]
    pub shell: Vec<String>,

    /// Extra KEY=VALUE environment for sync, install, and start; repeatable.
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Command that syncs the local checkout.
    #[arg(long)]
    pub sync: Option<String>,

    /// Command that installs the project.
    #[arg(long)]
    pub install: Option<String>,

    /// Command that starts the project.
    #[arg(long)]
    pub start: Option<String>,

    /// Number of successful deployments to retain.
    #[arg(long)]
    pub keep: Option<usize>,

    /// How to run releases: process (default) or container.
    #[arg(long, value_name = "KIND")]
    pub runtime: Option<String>,

    /// The port the application listens on; published by containers and
    /// passed as PORT.
    #[arg(long)]
    pub port: Option<u16>,

    /// Base image for the generated Dockerfile.
    #[arg(long, value_name = "IMAGE")]
    pub base: Option<String>,

    /// Dockerfile in the repository to build instead of generating one.
    #[arg(long, value_name = "PATH")]
    pub dockerfile: Option<String>,

    /// Sync, install, and start one release as a test, verify it stays up
    /// briefly, then stop it and exit without touching the deployed state.
    #[arg(long, conflicts_with_all = ["status", "rollback"])]
    pub dry_run: bool,

    /// Show the deployed state and exit.
    #[arg(long, conflicts_with = "rollback")]
    pub status: bool,

    /// Repoint current at an earlier release and exit. Without a value, the
    /// release before the current one is used.
    #[arg(long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
    pub rollback: Option<String>,

    /// Plain line output instead of the live dashboard on interactive
    /// terminals.
    #[arg(long)]
    pub no_tui: bool,
}

/// A `.tinycd/config.toml`: the local one in the tracked directory, or the
/// one committed to the repository, of which only the recipe is used.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub name: Option<String>,
    pub poll: Option<u64>,
    /// Once a listen address; any value but false enables the webhook.
    pub hook: Option<toml::Value>,
    pub token: Option<String>,
    pub repo: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub share: Option<Vec<String>>,
    pub interlock: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub shell: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub sync: Option<String>,
    pub install: Option<String>,
    pub start: Option<String>,
    pub check: Option<String>,
    pub keep: Option<usize>,
    pub runtime: Option<String>,
    pub port: Option<u16>,
    pub base: Option<String>,
    pub dockerfile: Option<String>,
}

/// Setting names that signal a misplaced key inside the [env] table.
const SETTING_NAMES: [&str; 20] = [
    "name",
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
    "runtime",
    "port",
    "base",
    "dockerfile",
    "env",
];

impl FileConfig {
    pub async fn load(path: &Path, required: bool) -> Result<Self, Error> {
        let source = match tokio::fs::read_to_string(path).await {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display()).into()),
        };
        Self::parse(&source)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()).into())
    }

    pub fn parse(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(source)
    }

    /// The part of the file a repository may provide.
    pub fn recipe(&self) -> Recipe {
        Recipe {
            shell: self.shell.clone(),
            env: self.env.clone(),
            sync: self.sync.clone(),
            install: self.install.clone(),
            start: self.start.clone(),
            check: self.check.clone(),
            port: self.port,
            base: self.base.clone(),
            dockerfile: self.dockerfile.clone(),
        }
    }

    fn hook_enabled(&self) -> bool {
        match &self.hook {
            None => false,
            Some(toml::Value::Boolean(enabled)) => *enabled,
            Some(_) => true,
        }
    }
}

/// Where the repository and the deployment state come from.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Track a local checkout: poll its remote and keep deployments under
    /// ~/.tinycd so the working tree stays clean.
    Local { project: PathBuf },
    /// Deploy a Git URL: configuration and deployments live in home.
    Url { repo: String, home: PathBuf },
}

impl Mode {
    pub fn resolve(source: Option<&str>, dir: Option<&Path>) -> Result<Self, Error> {
        let Some(source) = source else {
            return Ok(Self::Local {
                project: project_dir(Path::new("."))?,
            });
        };
        if looks_like_git_url(source) {
            let home = dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            std::fs::create_dir_all(&home)
                .map_err(|error| format!("failed to create {}: {error}", home.display()))?;
            return Ok(Self::Url {
                repo: source.to_owned(),
                home: std::path::absolute(&home)?,
            });
        }
        if dir.is_some() {
            return Err("a second path is only accepted after a Git URL".into());
        }
        Ok(Self::Local {
            project: project_dir(Path::new(source))?,
        })
    }

    /// The directory whose .tinycd/config.toml applies.
    pub fn dir(&self) -> &Path {
        match self {
            Self::Local { project } => project,
            Self::Url { home, .. } => home,
        }
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
pub fn looks_like_git_url(source: &str) -> bool {
    source.contains("://")
        || source
            .split_once(':')
            .is_some_and(|(host, _)| host.len() > 1 && !host.contains('/') && !host.contains('\\'))
}

/// The project id for a directory: its name plus a short digest of its
/// absolute path, so restarts find the same releases.
pub fn project_key(dir: &Path) -> String {
    let name: String = dir
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
    let digest = Sha256::digest(dir.as_os_str().as_encoded_bytes());
    format!(
        "{name}-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

/// Deployments for a tracked checkout live in the data directory
/// (~/.tinycd by default), keyed by the checkout's absolute path, so the
/// working tree stays clean and a restart finds the same releases.
pub fn state_root(data: &Path, project: &Path) -> PathBuf {
    data.join(project_key(project))
}

/// Record the deployed repository next to the deployments so a plain `tinycd`
/// run there restarts the same deployment. Files tinycd wrote earlier are
/// refreshed when the URL changes; files the user authored are left alone.
pub fn write_local_config(home: &Path, repo: &str) -> Result<(), Error> {
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

/// Merge the command line over the local config file over the defaults into
/// a project on the local host. Settings that describe the project itself
/// may also come from the .tinycd/config.toml inside the repository; those
/// are merged per release by the agent.
pub async fn quick_project(args: &QuickArgs, mode: &Mode, data: &Path) -> Result<Project, Error> {
    if args.repo.is_some() && matches!(mode, Mode::Url { .. }) {
        return Err("pass the repository as either the first argument or --repo, not both".into());
    }
    if let (Mode::Url { repo, home }, None) = (mode, &args.config) {
        write_local_config(home, repo)?;
    }
    let dir = mode.dir().to_path_buf();

    let required = args.config.is_some();
    let path = args
        .config
        .clone()
        .unwrap_or_else(|| dir.join(".tinycd").join("config.toml"));
    let file_dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
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

    let mut poll = args.poll.or(file.poll);
    let hook = args.hook.is_some() || file.hook_enabled();
    if poll.is_none() && !hook {
        poll = Some(30);
    }
    if poll == Some(0) {
        return Err("poll must be at least one second".into());
    }

    let token = args.token.clone().or_else(|| file.token.clone());
    if hook && token.is_none() {
        return Err(
            "hook mode requires --token, TINYCD_TOKEN, or token in .tinycd/config.toml".into(),
        );
    }
    if token
        .as_ref()
        .is_some_and(|token| token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic()))
    {
        return Err("the webhook token must be at least 32 printable non-space ASCII bytes".into());
    }

    // Relative paths in the file are relative to the file; relative CLI
    // paths are relative to the working directory.
    let resolve = |path: PathBuf| {
        let path = if path.is_absolute() {
            path
        } else {
            file_dir.join(path)
        };
        std::path::absolute(&path).unwrap_or(path)
    };
    let root = match (&args.root, &file.root) {
        (Some(root), _) => std::path::absolute(root).unwrap_or_else(|_| root.clone()),
        (None, Some(root)) => resolve(root.clone()),
        (None, None) => match mode {
            Mode::Local { project } => state_root(data, project),
            Mode::Url { home, .. } => home.join(".tinycd"),
        },
    };
    let interlock = args
        .interlock
        .clone()
        .or_else(|| file.interlock.clone().map(resolve))
        .unwrap_or_else(|| root.join("interlock"));

    let shell = if args.shell.is_empty() {
        file.shell.clone()
    } else {
        Some(args.shell.clone())
    };
    if shell.as_deref().is_some_and(|shell| shell.is_empty()) {
        return Err("shell must name a program".into());
    }

    let mut env = file.env.clone();
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
    for pair in &args.env {
        match pair.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                env.insert(key.to_owned(), value.to_owned());
            }
            _ => return Err(format!("--env {pair} must look like KEY=VALUE").into()),
        }
    }

    let keep = args.keep.or(file.keep).unwrap_or(5);
    if keep == 0 {
        return Err("keep must be at least one deployment".into());
    }

    let repo = match mode {
        Mode::Url { repo, .. } => repo.clone(),
        Mode::Local { project } => {
            crate::git::repo_url(args.repo.clone().or_else(|| file.repo.clone()), project).await?
        }
    };

    let mut share = if args.share.is_empty() {
        file.share.clone().unwrap_or_default()
    } else {
        args.share.clone()
    };
    validate_share(&mut share)?;

    let runtime = match args.runtime.as_deref().or(file.runtime.as_deref()) {
        None => RuntimeKind::Process,
        Some(text) => RuntimeKind::parse(text)
            .ok_or_else(|| format!("runtime must be process or container, not {text:?}"))?,
    };

    let id = util::slug(&project_key(&dir));
    let name = file.name.clone().unwrap_or_else(|| {
        dir.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "project".to_owned())
    });

    Ok(Project {
        id,
        name,
        repo,
        git_ref: args.git_ref.clone().or_else(|| file.git_ref.clone()),
        host: LOCAL_HOST.to_owned(),
        runtime,
        recipe: Recipe {
            shell,
            env,
            sync: args.sync.clone().or_else(|| file.sync.clone()),
            install: args.install.clone().or_else(|| file.install.clone()),
            start: args.start.clone().or_else(|| file.start.clone()),
            check: args.check.clone().or_else(|| file.check.clone()),
            port: args.port.or(file.port),
            base: args.base.clone().or_else(|| file.base.clone()),
            dockerfile: args.dockerfile.clone().or_else(|| file.dockerfile.clone()),
        },
        share,
        keep,
        poll,
        webhook_token: if hook { token } else { None },
        root: Some(root),
        interlock: Some(interlock),
        current: None,
        running: false,
        head: None,
        created_at: util::now_ms(),
    })
}

/// Share entries must be relative paths without any parent components.
pub fn validate_share(share: &mut [String]) -> Result<(), Error> {
    for entry in share.iter_mut() {
        let trimmed = entry.trim_end_matches(['/', '\\']);
        if trimmed.is_empty()
            || Path::new(trimmed)
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(format!("share entry {entry:?} must be a relative path").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_urls_are_recognised() {
        assert!(looks_like_git_url("git@github.com:e/app.git"));
        assert!(looks_like_git_url("https://github.com/e/app.git"));
        assert!(!looks_like_git_url("C:\\app"));
        assert!(!looks_like_git_url("./app"));
    }

    #[test]
    fn legacy_hook_addresses_still_parse() {
        let file = FileConfig::parse("hook = \"127.0.0.1:8080\"\nstart = \"x\"").unwrap();
        assert!(file.hook_enabled());
        let file = FileConfig::parse("hook = false").unwrap();
        assert!(!file.hook_enabled());
        assert!(FileConfig::parse("bogus = 1").is_err());
    }

    #[test]
    fn project_keys_are_stable() {
        let key = project_key(Path::new("/home/me/My App"));
        assert!(key.starts_with("My-App-"));
        assert_eq!(key.len(), "My-App-".len() + 8);
    }
}
