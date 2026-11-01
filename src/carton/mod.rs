//! A carton is one running release of a project. The pipeline builds and
//! starts cartons through a runtime and never cares which: a plain
//! process, or an OCI container through docker, podman, or nerdctl.

pub mod oci;
pub mod process;

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use tokio::process::Command;

use crate::util::Error;

/// How long a stopping carton gets to exit gracefully before it is killed.
pub const GRACE_PERIOD: Duration = Duration::from_secs(10);

/// Everything a runtime needs to build and run one release, fully resolved.
#[derive(Clone, Debug)]
pub struct CartonSpec {
    pub project: String,
    pub deployment: String,
    pub shell: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub install: Option<String>,
    pub start: String,
    pub check: Option<String>,
    pub port: Option<u16>,
    pub base: Option<String>,
    pub dockerfile: Option<String>,
    /// Untracked paths shared across releases, and where they live.
    pub share: Vec<String>,
    pub shared_dir: PathBuf,
}

pub enum Runtime {
    Process(process::ProcessRuntime),
    Oci(oci::OciRuntime),
}

/// A carton that is running, or was: the handle to wait on and stop.
pub enum Carton {
    Process(process::Server),
    Oci(oci::Container),
}

impl Runtime {
    /// Turn a synced release folder into something that can start: run the
    /// install command, or build an image.
    pub async fn build(
        &self,
        release: &std::path::Path,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<(), Error> {
        match self {
            Self::Process(runtime) => runtime.build(release, spec, log).await,
            Self::Oci(runtime) => runtime.build(release, spec, log).await,
        }
    }

    pub async fn start(
        &self,
        release: &std::path::Path,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<Carton, Error> {
        match self {
            Self::Process(runtime) => runtime.start(release, spec, log).map(Carton::Process),
            Self::Oci(runtime) => runtime.start(spec, log).await.map(Carton::Oci),
        }
    }

    /// Reattach to a carton that outlived the agent, if the runtime keeps
    /// them alive: containers do, processes do not.
    pub async fn adopt(
        &self,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<Option<Carton>, Error> {
        match self {
            Self::Process(_) => Ok(None),
            Self::Oci(runtime) => runtime.adopt(spec, log).await.map(|c| c.map(Carton::Oci)),
        }
    }

    /// Whether a release can start without being built again.
    pub async fn built(&self, release: &std::path::Path, spec: &CartonSpec) -> bool {
        match self {
            Self::Process(_) => release.is_dir(),
            Self::Oci(runtime) => runtime.image_exists(spec).await,
        }
    }

    /// Drop build products of deployments that are no longer retained.
    pub async fn prune(&self, project: &str, keep: &[String]) {
        if let Self::Oci(runtime) = self {
            runtime.prune(project, keep).await;
        }
    }
}

impl Carton {
    /// Resolve when the carton exits, with a description of how.
    pub async fn wait(&mut self) -> Result<String, Error> {
        match self {
            Self::Process(server) => server.wait().await,
            Self::Oci(container) => container.wait().await,
        }
    }

    /// The exit description if the carton has already exited.
    pub async fn exited(&mut self) -> Result<Option<String>, Error> {
        match self {
            Self::Process(server) => server.exited(),
            Self::Oci(container) => container.exited().await,
        }
    }

    /// Stop the carton: ask politely, wait up to GRACE_PERIOD, then kill.
    pub async fn stop(self) -> Result<(), Error> {
        match self {
            Self::Process(server) => server.stop().await,
            Self::Oci(container) => container.stop().await,
        }
    }

    /// Explicitly relinquish control when a project is removed with
    /// --keep-running. This differs from normal agent shutdown.
    pub async fn detach(self) -> Result<(), Error> {
        match self {
            Self::Process(server) => server.detach(),
            Self::Oci(container) => {
                container.detach().await;
                Ok(())
            }
        }
    }

    /// Let go of the carton when the agent itself stops. A process cannot
    /// outlive its parent and is stopped; a container keeps running under
    /// its engine and is adopted when the agent returns. Returns what was
    /// left running.
    pub async fn release(self) -> Result<Option<String>, Error> {
        match self {
            Self::Process(server) => server.stop().await.map(|()| None),
            Self::Oci(container) => Ok(Some(container.detach().await)),
        }
    }
}

pub fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".to_owned(), "/C".to_owned()]
    } else {
        vec!["sh".to_owned(), "-c".to_owned()]
    }
}

/// A command line run through the given shell with the given environment.
/// Deployment commands never inherit tinycd's own secrets.
pub fn shell_command(
    shell: &[String],
    env: &BTreeMap<String, String>,
    line: &str,
    dir: &std::path::Path,
) -> Command {
    let (program, args) = shell
        .split_first()
        .unwrap_or_else(|| panic!("shell is validated"));
    let mut command = Command::new(program);
    command
        .args(args)
        .arg(line)
        .current_dir(dir)
        .env_remove("TINYCD_TOKEN")
        .env_remove("TINYCD_JOIN_TOKEN")
        .env_remove("TINYCD_REPO")
        .env_remove("TINYCD_REF")
        .envs(env);
    command
}

/// Send a command's output to the release log instead of tinycd's stdout.
pub fn redirect(command: &mut Command, log: Option<&std::fs::File>) -> Result<(), Error> {
    if let Some(log) = log {
        command
            .stdout(std::process::Stdio::from(log.try_clone()?))
            .stderr(std::process::Stdio::from(log.try_clone()?));
    }
    Ok(())
}

/// Record a pipeline step in the release log.
pub fn log_line(log: Option<&std::fs::File>, line: &str) {
    if let Some(mut log) = log {
        use std::io::Write;
        let _ = writeln!(log, "{line}");
    }
}
