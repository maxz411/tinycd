//! The OCI runtime: releases become images and run as containers through
//! any docker-compatible command line. Nothing here is docker-specific, so
//! podman and nerdctl work by pointing `cli` at them. Containers restart
//! with the machine and outlive the agent, which adopts them on startup.

use std::path::Path;

use tokio::process::{Child, Command};

use super::{CartonSpec, GRACE_PERIOD, log_line, redirect};
use crate::util::{Error, say};

/// Command lines tried, in order, when none is configured.
const CANDIDATES: [&str; 3] = ["docker", "podman", "nerdctl"];

/// Where the repository lands inside the generated image.
const WORKDIR: &str = "/app";

#[derive(Clone)]
pub struct OciRuntime {
    cli: String,
    namespace: String,
}

impl OciRuntime {
    /// Use a specific command line, such as podman.
    fn new(cli: impl Into<String>, data: &Path) -> Self {
        let absolute = std::path::absolute(data).unwrap_or_else(|_| data.to_path_buf());
        Self {
            cli: cli.into(),
            namespace: crate::config::project_key(&absolute).to_ascii_lowercase(),
        }
    }

    pub fn cli(&self) -> &str {
        &self.cli
    }

    /// Find a working container command line: TINYCD_OCI if set, else the
    /// first candidate whose daemon answers.
    pub async fn detect(data: &Path) -> Option<Self> {
        let configured = std::env::var("TINYCD_OCI")
            .ok()
            .filter(|cli| !cli.is_empty());
        let candidates: Vec<String> = match configured {
            Some(cli) => vec![cli],
            None => CANDIDATES.iter().map(|cli| (*cli).to_owned()).collect(),
        };
        for cli in candidates {
            let runtime = Self::new(cli, data);
            if runtime.available().await {
                return Some(runtime);
            }
        }
        None
    }

    /// Whether the command line exists and its daemon answers.
    pub async fn available(&self) -> bool {
        let probe = Command::new(&self.cli)
            .args(["version", "--format", "{{.Server.Version}}"])
            .stdin(std::process::Stdio::null())
            .output();
        matches!(tokio::time::timeout(std::time::Duration::from_secs(20), probe).await,
            Ok(Ok(output)) if output.status.success())
    }

    fn image(&self, spec: &CartonSpec) -> String {
        format!(
            "tinycd/{}/{}:{}",
            self.namespace, spec.project, spec.deployment
        )
    }

    fn container(&self, spec: &CartonSpec) -> String {
        self.container_name(&spec.project, &spec.deployment)
    }

    fn container_name(&self, project: &str, deployment: &str) -> String {
        format!("tinycd-{}-{project}-{deployment}", self.namespace)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.cli);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Build the release into an image, generating a Dockerfile from the
    /// recipe unless the repository provides one.
    pub async fn build(
        &self,
        release: &Path,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<(), Error> {
        let dockerfile = match &spec.dockerfile {
            Some(path) => release.join(path),
            None if release.join("Dockerfile").is_file() && spec.install.is_none() => {
                release.join("Dockerfile")
            }
            None => {
                let path = release.join("Dockerfile.tinycd");
                std::fs::write(&path, generate_dockerfile(release, spec))?;
                path
            }
        };
        let image = self.image(spec);
        let line = format!("building image {image} from {}", dockerfile.display());
        log_line(log, &line);
        say(line);

        let mut process = self.command(&["build", "--file"]);
        process
            .arg(&dockerfile)
            .args(["--tag", &image])
            .arg(release)
            .current_dir(release);
        redirect(&mut process, log)?;
        let status = process.status().await?;
        if !status.success() {
            return Err(format!("{} build exited with {status}", self.cli).into());
        }
        Ok(())
    }

    pub async fn image_exists(&self, spec: &CartonSpec) -> bool {
        self.command(&["image", "inspect", &self.image(spec)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success())
    }

    /// Run the release's image as a detached container and follow its logs
    /// into the release log.
    pub async fn start(
        &self,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<Container, Error> {
        let name = self.container(spec);
        let image = self.image(spec);
        // A container left behind by an earlier run of this deployment
        // would block the name.
        let _ = self
            .command(&["rm", "--force", &name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        let mut run = self.command(&["run", "--detach", "--name", &name]);
        run.args(["--restart", "unless-stopped"]);
        run.args(["--label", "managed-by=tinycd"]);
        run.args(["--label", &format!("tinycd.project={}", spec.project)]);
        if let Some(port) = spec.port {
            run.args(["--publish", &format!("{port}:{port}")]);
        }
        for (key, value) in &spec.env {
            run.args(["--env", &format!("{key}={value}")]);
        }
        for entry in &spec.share {
            let relative = entry.trim_end_matches(['/', '\\']);
            let source = spec.shared_dir.join(relative);
            if !source.exists() {
                if entry.ends_with(['/', '\\']) {
                    std::fs::create_dir_all(&source)?;
                } else {
                    if let Some(parent) = source.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::File::create(&source)?;
                }
            }
            let source = std::path::absolute(&source)?;
            run.args([
                "--volume",
                &format!("{}:{WORKDIR}/{relative}", source.display()),
            ]);
        }
        run.arg(&image);

        let output = run.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log_line(log, stderr.trim());
            return Err(format!(
                "{} run exited with {}: {}",
                self.cli,
                output.status,
                stderr.trim()
            )
            .into());
        }

        let logs = self.follow_logs(&name, log)?;
        Ok(Container {
            cli: self.cli.clone(),
            name,
            logs,
        })
    }

    /// Reattach to the deployment's container if it is still running.
    pub async fn adopt(
        &self,
        spec: &CartonSpec,
        log: Option<&std::fs::File>,
    ) -> Result<Option<Container>, Error> {
        let name = self.container(spec);
        if !self.running(&name).await {
            return Ok(None);
        }
        let logs = self.follow_logs(&name, log)?;
        Ok(Some(Container {
            cli: self.cli.clone(),
            name,
            logs,
        }))
    }

    async fn running(&self, name: &str) -> bool {
        let output = self
            .command(&["inspect", "--format", "{{.State.Running}}", name])
            .output()
            .await;
        matches!(output, Ok(output) if output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() == "true")
    }

    fn follow_logs(&self, name: &str, log: Option<&std::fs::File>) -> Result<Option<Child>, Error> {
        let Some(log) = log else {
            return Ok(None);
        };
        let mut follow = self.command(&["logs", "--follow", "--since", "0s", name]);
        redirect(&mut follow, Some(log))?;
        follow.kill_on_drop(true);
        Ok(Some(follow.spawn()?))
    }

    /// Remove images and stopped containers of deployments that are no
    /// longer retained.
    pub async fn prune(&self, project: &str, keep: &[String]) {
        let repository = format!("tinycd/{}/{project}", self.namespace);
        let prefix = format!("{repository}:");
        if let Ok(output) = self
            .command(&[
                "images",
                "--format",
                "{{.Repository}}:{{.Tag}}",
                &repository,
            ])
            .output()
            .await
        {
            for image in String::from_utf8_lossy(&output.stdout).lines() {
                let Some(id) = image.strip_prefix(&prefix) else {
                    continue;
                };
                if keep.iter().any(|kept| kept == id) {
                    continue;
                }
                let _ = self
                    .command(&["rm", "--force", &self.container_name(project, id)])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
                if let Ok(status) = self
                    .command(&["image", "rm", image])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    && !status.success()
                {
                    crate::util::say_err(format!("failed to remove image {image}"));
                }
            }
        }
    }
}

/// A running container and the process following its logs.
pub struct Container {
    cli: String,
    name: String,
    logs: Option<Child>,
}

impl Container {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.cli);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Block until the container exits. `docker wait` prints the exit code.
    pub async fn wait(&mut self) -> Result<String, Error> {
        let output = self.command(&["wait", &self.name]).output().await?;
        if !output.status.success() {
            return Err(format!(
                "{} wait failed: {}",
                self.cli,
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
        Ok(format!(
            "the container exited with code {}",
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }

    pub async fn exited(&mut self) -> Result<Option<String>, Error> {
        let output = self
            .command(&[
                "inspect",
                "--format",
                "{{.State.Running}} {{.State.ExitCode}}",
                &self.name,
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Ok(Some("the container is gone".to_owned()));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("true"), _) => Ok(None),
            (_, code) => Ok(Some(format!(
                "the container exited with code {}",
                code.unwrap_or("?")
            ))),
        }
    }

    /// Stop following the logs and leave the container running. Returns
    /// its name.
    pub async fn detach(mut self) -> String {
        if let Some(mut logs) = self.logs.take() {
            let _ = logs.kill().await;
        }
        self.name.clone()
    }

    /// Stop the container gracefully, then remove it so the name is free
    /// for a rollback; the image stays for that.
    pub async fn stop(mut self) -> Result<(), Error> {
        let timeout = GRACE_PERIOD.as_secs().to_string();
        let status = self
            .command(&["stop", "--time", &timeout, &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await?;
        if !status.success() {
            crate::util::say_err(format!("failed to stop container {}", self.name));
        }
        let _ = self
            .command(&["rm", "--force", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if let Some(mut logs) = self.logs.take() {
            let _ = logs.kill().await;
        }
        Ok(())
    }
}

/// The base image for a release with no Dockerfile: the configured one, or
/// a guess from the project's files.
pub fn detect_base(release: &Path) -> &'static str {
    let found = |file: &str| release.join(file).exists();
    if found("package.json") {
        "node:22-slim"
    } else if found("Cargo.toml") {
        "rust:1-slim"
    } else if found("go.mod") {
        "golang:1"
    } else if found("requirements.txt") || found("pyproject.toml") {
        "python:3-slim"
    } else if found("Gemfile") {
        "ruby:3-slim"
    } else if found("composer.json") {
        "php:8-cli"
    } else {
        "debian:stable-slim"
    }
}

/// Wrap the recipe in a Dockerfile: copy the release in, run install, and
/// start through the shell. The environment is applied at run time so
/// secrets never land in image layers.
pub fn generate_dockerfile(release: &Path, spec: &CartonSpec) -> String {
    let base = spec.base.as_deref().unwrap_or_else(|| detect_base(release));
    let mut out = format!("FROM {base}\nWORKDIR {WORKDIR}\nCOPY . {WORKDIR}\n");
    if let Some(install) = &spec.install {
        out.push_str(&format!("RUN {install}\n"));
    }
    if let Some(port) = spec.port {
        out.push_str(&format!("ENV PORT={port}\nEXPOSE {port}\n"));
    }
    let mut cmd: Vec<String> = spec.shell.clone();
    cmd.push(spec.start.clone());
    out.push_str(&format!(
        "CMD {}\n",
        serde_json::to_string(&cmd).expect("strings serialize")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CartonSpec {
        CartonSpec {
            project: "app".into(),
            deployment: "1".into(),
            shell: vec!["sh".into(), "-c".into()],
            env: Default::default(),
            install: Some("npm ci".into()),
            start: "npm start".into(),
            check: None,
            port: Some(3000),
            base: None,
            dockerfile: None,
            share: Vec::new(),
            shared_dir: std::path::PathBuf::from("/tmp/shared"),
        }
    }

    #[test]
    fn dockerfile_wraps_the_recipe() {
        let dir = std::env::temp_dir().join(format!("tinycd-oci-{}", crate::util::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        let text = generate_dockerfile(&dir, &spec());
        assert_eq!(
            text,
            "FROM node:22-slim\nWORKDIR /app\nCOPY . /app\nRUN npm ci\nENV PORT=3000\nEXPOSE 3000\nCMD [\"sh\",\"-c\",\"npm start\"]\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn configured_base_wins() {
        let mut spec = spec();
        spec.base = Some("node:20".into());
        spec.install = None;
        spec.port = None;
        let text = generate_dockerfile(Path::new("/nonexistent"), &spec);
        assert!(text.starts_with("FROM node:20\n"));
        assert!(!text.contains("RUN"));
        assert!(!text.contains("EXPOSE"));
    }
}
