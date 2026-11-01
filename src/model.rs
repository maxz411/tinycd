//! The records the hub keeps: settings, hosts, projects, deployments, and
//! the events it broadcasts to dashboards.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// What runs a carton: a plain process, or an OCI container through a
/// docker-compatible command line (docker, podman, nerdctl).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Process,
    Oci,
}

impl RuntimeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Oci => "container",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "process" | "proc" => Some(Self::Process),
            "oci" | "container" | "docker" | "podman" | "nerdctl" => Some(Self::Oci),
            _ => None,
        }
    }
}

/// How a release is built and run. The repository's `.tinycd/config.toml`
/// provides one of these, and the project's own recipe overrides it key by
/// key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recipe {
    pub shell: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub sync: Option<String>,
    pub install: Option<String>,
    pub start: Option<String>,
    pub check: Option<String>,
    /// The port the application listens on. Containers publish it, and the
    /// application receives it as PORT.
    pub port: Option<u16>,
    /// Base image for a generated Dockerfile.
    pub base: Option<String>,
    /// Dockerfile inside the repository to build instead of generating one.
    pub dockerfile: Option<String>,
}

impl Recipe {
    /// Layer `over` on top of this recipe: set keys in `over` win, and the
    /// environment tables merge with `over` winning per key.
    pub fn overridden_by(&self, over: &Recipe) -> Recipe {
        let mut env = self.env.clone();
        env.extend(over.env.clone());
        Recipe {
            shell: over.shell.clone().or_else(|| self.shell.clone()),
            env,
            sync: over.sync.clone().or_else(|| self.sync.clone()),
            install: over.install.clone().or_else(|| self.install.clone()),
            start: over.start.clone().or_else(|| self.start.clone()),
            check: over.check.clone().or_else(|| self.check.clone()),
            port: over.port.or(self.port),
            base: over.base.clone().or_else(|| self.base.clone()),
            dockerfile: over.dockerfile.clone().or_else(|| self.dockerfile.clone()),
        }
    }
}

/// A deployed repository: what to watch, where to run it, and how.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// The host that runs it.
    pub host: String,
    pub runtime: RuntimeKind,
    /// Dashboard-side overrides of the repository's recipe.
    pub recipe: Recipe,
    /// Untracked paths every release shares.
    pub share: Vec<String>,
    /// Successful releases to retain.
    pub keep: usize,
    /// Poll interval in seconds; None disables polling.
    pub poll: Option<u64>,
    /// Secret for the project's generic webhook at /api/hooks/<id>.
    pub webhook_token: Option<String>,
    /// Where the host keeps releases; the host's data directory when unset.
    /// Only honoured by the local host.
    pub root: Option<PathBuf>,
    /// File that pauses syncs and restarts while it exists.
    pub interlock: Option<PathBuf>,
    /// The live deployment, as last reported by the host.
    pub current: Option<String>,
    /// Whether the host reports the live deployment as running.
    pub running: bool,
    /// The commit last deployed or attempted; polling compares the remote
    /// against it.
    pub head: Option<String>,
    pub created_at: u64,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            repo: String::new(),
            git_ref: None,
            host: LOCAL_HOST.to_owned(),
            runtime: RuntimeKind::Process,
            recipe: Recipe::default(),
            share: Vec::new(),
            keep: 5,
            poll: Some(30),
            webhook_token: None,
            root: None,
            interlock: None,
            current: None,
            running: false,
            head: None,
            created_at: 0,
        }
    }
}

/// The id of the host that runs inside the hub's own process.
pub const LOCAL_HOST: &str = "local";

/// Where the dashboard listens unless told otherwise.
pub const DEFAULT_BIND: &str = "127.0.0.1:4000";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Syncing,
    Building,
    Starting,
    Checking,
    Live,
    /// Replaced by a newer live deployment; still retained for rollback
    /// until pruned.
    Retired,
    Failed,
    /// Coalesced away by a newer queued deployment.
    Cancelled,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Syncing => "syncing",
            Self::Building => "building",
            Self::Starting => "starting",
            Self::Checking => "checking",
            Self::Live => "live",
            Self::Retired => "retired",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the deployment is still being worked on.
    pub fn in_progress(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Syncing | Self::Building | Self::Starting | Self::Checking
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Deployment {
    pub id: String,
    pub project: String,
    pub commit: String,
    /// What caused it: poll, webhook, manual, rollback, resume.
    pub trigger: String,
    pub status: Status,
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Still present on the host, so it can be rolled back to.
    pub retained: bool,
}

impl Default for Deployment {
    fn default() -> Self {
        Self {
            id: String::new(),
            project: String::new(),
            commit: String::new(),
            trigger: "manual".to_owned(),
            status: Status::Queued,
            error: None,
            created_at: 0,
            updated_at: 0,
            retained: true,
        }
    }
}

/// A machine that runs cartons. The local host is the hub's own process.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Host {
    pub id: String,
    pub name: String,
    /// Digest of the agent's bearer token; empty for the local host.
    pub token_hash: String,
    pub local: bool,
    /// Runtimes the agent last reported as usable.
    pub runtimes: Vec<RuntimeKind>,
    pub os: String,
    pub version: String,
    pub last_seen: Option<u64>,
    pub created_at: u64,
}

/// A one-time token a new host redeems to join.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JoinToken {
    pub name: String,
    pub secret_hash: String,
    pub expires_at: u64,
}

/// Credentials of the GitHub App created through the manifest flow.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GitHubApp {
    pub id: u64,
    pub slug: String,
    pub name: String,
    pub owner: String,
    pub html_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub webhook_secret: String,
    pub pem: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// PBKDF2 hash of the dashboard password; unset until first setup.
    pub admin_password: Option<String>,
    /// The plain HTTP listener for this machine's browser and CLI.
    pub bind: String,
    /// Where GitHub and other machines reach the dashboard.
    pub public_url: Option<String>,
    /// HTTPS listener for agents and remote access, such as 0.0.0.0:4443.
    pub secure_bind: Option<String>,
    /// Default poll interval for new projects.
    pub poll: u64,
    pub github: Option<GitHubApp>,
    pub join_tokens: Vec<JoinToken>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            admin_password: None,
            bind: DEFAULT_BIND.to_owned(),
            public_url: None,
            secure_bind: None,
            poll: 30,
            github: None,
            join_tokens: Vec::new(),
        }
    }
}

/// What the hub tells dashboards, as it happens.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// A line for the activity feed.
    Message {
        text: String,
    },
    Log {
        project: String,
        deployment: String,
        line: String,
    },
    Deployment {
        deployment: Deployment,
    },
    Project {
        project: Project,
    },
    ProjectRemoved {
        project: String,
    },
    Host {
        host: String,
        online: bool,
    },
}

/// A snapshot of everything a dashboard shows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overview {
    pub version: String,
    pub started_at: u64,
    pub url: String,
    pub hosts: Vec<HostView>,
    pub projects: Vec<ProjectView>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostView {
    #[serde(flatten)]
    pub host: Host,
    pub online: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectView {
    #[serde(flatten)]
    pub project: Project,
    pub deployments: Vec<Deployment>,
    pub in_progress: Option<String>,
}
