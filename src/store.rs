//! The hub's persistent state: small JSON files under one data directory,
//! written atomically. The layout is meant to be read by hand:
//!
//! ```text
//! <data>/
//! ├── lock                 held by the running hub
//! ├── settings.json
//! ├── hosts.json
//! ├── api-token            bearer token for the CLI on this machine
//! ├── daemon.json          where the running hub listens
//! ├── tls/                 self-signed certificate for agents
//! ├── projects/<id>.json
//! ├── deployments/<project>/<id>.json
//! ├── logs/<project>/<id>.log
//! └── agent/               the local agent's releases
//! ```

use std::path::{Path, PathBuf};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    model::{Deployment, Host, Project, Settings},
    util::{self, Error},
};

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

/// Where the running hub can be reached from this machine.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DaemonInfo {
    pub url: String,
    pub pid: u32,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        let _ = util::restrict(dir, 0o700);
        for sub in ["projects", "deployments", "logs", "tls", "agent"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        Ok(Self {
            dir: std::path::absolute(dir)?,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the in-process agent keeps its releases.
    pub fn agent_dir(&self) -> PathBuf {
        self.dir.join("agent")
    }

    /// Hold an exclusive lock so a second hub cannot manage the same data.
    /// The lock is advisory and dies with the process.
    pub fn lock(&self) -> Result<std::fs::File, Error> {
        lock_file(&self.dir.join("lock"))
            .map_err(|error| format!("{error} (data directory {})", self.dir.display()).into())
    }

    /// Whether a hub currently holds the lock.
    pub fn locked(&self) -> bool {
        match std::fs::File::open(self.dir.join("lock")) {
            Ok(file) => matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            Err(_) => false,
        }
    }

    fn read<T: DeserializeOwned + Default>(&self, path: &Path) -> Result<T, Error> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()).into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(error) => Err(format!("failed to read {}: {error}", path.display()).into()),
        }
    }

    fn write<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(value)?;
        util::write_atomic(path, &bytes, 0o600)
            .map_err(|error| format!("failed to write {}: {error}", path.display()).into())
    }

    pub fn settings(&self) -> Result<Settings, Error> {
        self.read(&self.dir.join("settings.json"))
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), Error> {
        self.write(&self.dir.join("settings.json"), settings)
    }

    pub fn hosts(&self) -> Result<Vec<Host>, Error> {
        self.read(&self.dir.join("hosts.json"))
    }

    pub fn save_hosts(&self, hosts: &[Host]) -> Result<(), Error> {
        self.write(&self.dir.join("hosts.json"), &hosts)
    }

    pub fn projects(&self) -> Result<Vec<Project>, Error> {
        let mut projects = Vec::new();
        for entry in std::fs::read_dir(self.dir.join("projects"))? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                projects.push(self.read::<Project>(&path)?);
            }
        }
        projects.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(projects)
    }

    pub fn save_project(&self, project: &Project) -> Result<(), Error> {
        self.write(
            &self
                .dir
                .join("projects")
                .join(format!("{}.json", project.id)),
            project,
        )
    }

    pub fn delete_project(&self, id: &str) -> Result<(), Error> {
        let path = self.dir.join("projects").join(format!("{id}.json"));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("failed to remove {}: {error}", path.display()).into());
            }
        }
        let _ = std::fs::remove_dir_all(self.dir.join("deployments").join(id));
        let _ = std::fs::remove_dir_all(self.dir.join("logs").join(id));
        Ok(())
    }

    pub fn deployments(&self, project: &str) -> Result<Vec<Deployment>, Error> {
        let dir = self.dir.join("deployments").join(project);
        let mut deployments = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(deployments),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                deployments.push(self.read::<Deployment>(&path)?);
            }
        }
        deployments.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(deployments)
    }

    pub fn save_deployment(&self, deployment: &Deployment) -> Result<(), Error> {
        self.write(
            &self
                .dir
                .join("deployments")
                .join(&deployment.project)
                .join(format!("{}.json", deployment.id)),
            deployment,
        )
    }

    pub fn log_path(&self, project: &str, deployment: &str) -> PathBuf {
        self.dir
            .join("logs")
            .join(project)
            .join(format!("{deployment}.log"))
    }

    /// Append a line to the hub's copy of a deployment log.
    pub fn append_log(&self, project: &str, deployment: &str, line: &str) {
        use std::io::Write;

        let path = self.log_path(project, deployment);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    /// The bearer token the CLI on this machine uses, created on first use.
    pub fn api_token(&self) -> Result<String, Error> {
        let path = self.dir.join("api-token");
        match std::fs::read_to_string(&path) {
            Ok(token) if token.trim().len() >= 32 => Ok(token.trim().to_owned()),
            Ok(_) | Err(_) => {
                let token = util::random_hex(32);
                util::write_atomic(&path, token.as_bytes(), 0o600)
                    .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
                Ok(token)
            }
        }
    }

    pub fn daemon(&self) -> Option<DaemonInfo> {
        let bytes = std::fs::read(self.dir.join("daemon.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save_daemon(&self, info: &DaemonInfo) -> Result<(), Error> {
        self.write(&self.dir.join("daemon.json"), info)
    }

    pub fn clear_daemon(&self) {
        let _ = std::fs::remove_file(self.dir.join("daemon.json"));
    }

    pub fn sessions(&self) -> Vec<String> {
        self.read(&self.dir.join("sessions.json"))
            .unwrap_or_default()
    }

    pub fn save_sessions(&self, sessions: &[String]) {
        let _ = self.write(&self.dir.join("sessions.json"), &sessions);
    }

    pub fn tls_dir(&self) -> PathBuf {
        self.dir.join("tls")
    }
}

/// The default data directory: $TINYCD_HOME, else /var/lib/tinycd when
/// running as root on Unix, else ~/.tinycd.
pub fn default_data_dir() -> Result<PathBuf, Error> {
    if let Some(dir) = std::env::var_os("TINYCD_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        return Ok(PathBuf::from("/var/lib/tinycd"));
    }
    util::home_dir()
        .map(|home| home.join(".tinycd"))
        .ok_or_else(|| "HOME must be set to locate the data directory; or set TINYCD_HOME".into())
}

/// Take an exclusive advisory lock on a file, creating it if needed.
pub fn lock_file(path: &Path) -> Result<std::fs::File, Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err(format!("another tinycd is already running with {}", path.display()).into())
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(format!("failed to lock {}: {error}", path.display()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tinycd-store-{}", util::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn projects_and_deployments_round_trip() {
        let dir = temp_dir();
        let store = Store::open(&dir).unwrap();
        let project = Project {
            id: "app".into(),
            name: "App".into(),
            repo: "https://example.com/app.git".into(),
            ..Project::default()
        };
        store.save_project(&project).unwrap();
        assert_eq!(store.projects().unwrap().len(), 1);

        let deployment = Deployment {
            id: "1".into(),
            project: "app".into(),
            commit: "abc".into(),
            ..Deployment::default()
        };
        store.save_deployment(&deployment).unwrap();
        assert_eq!(store.deployments("app").unwrap()[0].commit, "abc");
        assert!(store.deployments("other").unwrap().is_empty());

        store.delete_project("app").unwrap();
        assert!(store.projects().unwrap().is_empty());
        assert!(store.deployments("app").unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = temp_dir();
        let store = Store::open(&dir).unwrap();
        assert!(!store.locked());
        let _held = store.lock().unwrap();
        assert!(store.locked());
        assert!(store.lock().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
