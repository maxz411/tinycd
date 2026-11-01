//! An agent runs cartons on one host and takes orders from the hub. It is
//! the same code whether it shares the hub's process (the default single
//! machine setup), runs in a container next to it, or sits on another
//! machine behind NAT: only the link differs.

pub mod link;
pub mod pipeline;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as SyncMutex},
    time::Duration,
};

use tokio::sync::{Mutex, mpsc, watch};

use crate::{
    carton::{Carton, Runtime, oci::OciRuntime, process::ProcessRuntime},
    model::{Project, RuntimeKind, Status},
    protocol::{ToAgent, ToHub},
    util::{Error, say, say_err},
};

/// How often running cartons are checked for unexpected exits.
const MONITOR_INTERVAL: Duration = Duration::from_secs(5);

pub struct Agent {
    data: PathBuf,
    /// Whether the hub is in another process, so activity must be sent
    /// rather than just printed.
    remote: bool,
    outbox: mpsc::UnboundedSender<ToHub>,
    oci: Option<OciRuntime>,
    projects: SyncMutex<HashMap<String, Arc<Mutex<Runner>>>>,
    /// The last progress reported per project while a deployment is in
    /// flight, re-sent after a reconnect.
    inflight: SyncMutex<HashMap<String, ToHub>>,
}

/// One project on this host.
pub struct Runner {
    pub project: Project,
    pub root: PathBuf,
    pub running: Option<Carton>,
    pub current: Option<String>,
    pub head: Option<String>,
    /// Consecutive restarts after unexpected exits.
    pub restarts: u32,
    /// Stopped on purpose, so the monitor must not restart it.
    pub stopped: bool,
    tail: Option<watch::Sender<bool>>,
}

impl Runner {
    fn new(project: Project, root: PathBuf) -> Self {
        Self {
            project,
            root,
            running: None,
            current: None,
            head: None,
            restarts: 0,
            stopped: false,
            tail: None,
        }
    }

    pub fn interlock(&self) -> PathBuf {
        self.project
            .interlock
            .clone()
            .unwrap_or_else(|| self.root.join("interlock"))
    }
}

impl Agent {
    pub async fn new(data: PathBuf, remote: bool, outbox: mpsc::UnboundedSender<ToHub>) -> Self {
        std::fs::create_dir_all(&data).ok();
        let oci = OciRuntime::detect(&data).await;
        match &oci {
            Some(runtime) => say(format!("container runtime: {}", runtime.cli())),
            None => say("container runtime: none found; releases run as processes"),
        }
        Self {
            data,
            remote,
            outbox,
            oci,
            projects: SyncMutex::new(HashMap::new()),
            inflight: SyncMutex::new(HashMap::new()),
        }
    }

    pub fn runtimes(&self) -> Vec<RuntimeKind> {
        let mut kinds = vec![RuntimeKind::Process];
        if self.oci.is_some() {
            kinds.push(RuntimeKind::Oci);
        }
        kinds
    }

    pub fn register_message(&self, name: &str) -> ToHub {
        ToHub::Register {
            name: name.to_owned(),
            os: std::env::consts::OS.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            runtimes: self.runtimes(),
        }
    }

    pub fn runtime_for(&self, project: &Project) -> Result<Runtime, Error> {
        match project.runtime {
            RuntimeKind::Process => Ok(Runtime::Process(ProcessRuntime)),
            RuntimeKind::Oci => self.oci.clone().map(Runtime::Oci).ok_or_else(|| {
                "no container runtime (docker, podman, or nerdctl) answers on this host".into()
            }),
        }
    }

    fn send(&self, message: ToHub) {
        let _ = self.outbox.send(message);
    }

    /// An activity line: printed here, and sent to a hub elsewhere.
    pub fn note(&self, text: impl Into<String>) {
        let text = text.into();
        say(text.clone());
        if self.remote {
            self.send(ToHub::Note { text });
        }
    }

    pub fn progress(
        &self,
        project: &str,
        deployment: &str,
        status: Status,
        commit: Option<String>,
        error: Option<String>,
    ) {
        let message = ToHub::Progress {
            project: project.to_owned(),
            deployment: deployment.to_owned(),
            status,
            commit,
            error,
        };
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if status.in_progress() {
            inflight.insert(project.to_owned(), message.clone());
        } else {
            inflight.remove(project);
        }
        drop(inflight);
        self.send(message);
    }

    async fn report_state(&self, runner: &Runner) {
        self.send(ToHub::State {
            project: runner.project.id.clone(),
            current: runner.current.clone(),
            running: runner.running.is_some(),
            retained: pipeline::retained(&runner.root).await,
        });
    }

    /// Stream a deployment's log to the hub as it grows, replacing any
    /// earlier tail for the project.
    pub fn tail_log(&self, runner: &mut Runner, deployment: &str, from_start: bool) {
        if let Some(previous) = runner.tail.take() {
            let _ = previous.send(true);
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        runner.tail = Some(stop_tx);
        tokio::spawn(tail(
            pipeline::release_log(&runner.root, deployment),
            runner.project.id.clone(),
            deployment.to_owned(),
            self.outbox.clone(),
            stop_rx,
            from_start,
        ));
    }

    /// Everything the hub should know after a (re)connection.
    pub async fn resync(&self) {
        let runners: Vec<_> = self
            .projects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for runner in runners {
            if let Ok(runner) = runner.try_lock() {
                self.report_state(&runner).await;
            }
        }
        let inflight: Vec<ToHub> = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for message in inflight {
            self.send(message);
        }
    }

    fn runner(&self, project: &str) -> Option<Arc<Mutex<Runner>>> {
        self.projects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .cloned()
    }

    async fn assign(self: &Arc<Self>, project: Project) {
        let root = project
            .root
            .clone()
            .filter(|_| !self.remote)
            .unwrap_or_else(|| self.data.join(&project.id));
        let (runner, fresh) = {
            let mut projects = self.projects.lock().unwrap_or_else(|e| e.into_inner());
            match projects.get(&project.id) {
                Some(runner) => (runner.clone(), false),
                None => {
                    let runner = Arc::new(Mutex::new(Runner::new(project.clone(), root.clone())));
                    projects.insert(project.id.clone(), runner.clone());
                    (runner, true)
                }
            }
        };
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            let mut guard = runner.lock().await;
            guard.project = project;
            if fresh {
                if let Err(error) = std::fs::create_dir_all(&root) {
                    agent.note(format!("failed to create {}: {error}", root.display()));
                }
                say(format!(
                    "project {}: {} in {}",
                    guard.project.id,
                    guard.project.repo,
                    root.display()
                ));
                if let Err(error) = pipeline::resume(&agent, &mut guard).await {
                    agent.note(format!("failed to resume {}: {error}", guard.project.id));
                }
            }
            agent.report_state(&guard).await;
        });
    }

    fn unassign(&self, project: &str, stop: bool) {
        let removed = self
            .projects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(project);
        if let Some(runner) = removed {
            tokio::spawn(async move {
                let mut runner = runner.lock().await;
                runner.stopped = true;
                if let Some(tail) = runner.tail.take() {
                    let _ = tail.send(true);
                }
                if let Some(carton) = runner.running.take() {
                    let result = if stop {
                        carton.stop().await
                    } else {
                        carton.detach().await
                    };
                    if let Err(error) = result {
                        say_err(format!("failed to release {}: {error}", runner.project.id));
                    }
                }
            });
        }
    }

    /// Act on one order from the hub. Long work runs on its own task so the
    /// link keeps flowing; work on one project is serialised by its runner.
    pub async fn handle(self: &Arc<Self>, message: ToAgent) {
        match message {
            ToAgent::Welcome { projects, .. } => {
                let stale: Vec<String> = self
                    .projects
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys()
                    .filter(|id| !projects.iter().any(|p| p.id == **id))
                    .cloned()
                    .collect();
                for id in stale {
                    self.unassign(&id, true);
                }
                for project in projects {
                    self.assign(project).await;
                }
            }
            ToAgent::Assign { project } => self.assign(project).await,
            ToAgent::Unassign { project, stop } => self.unassign(&project, stop),
            ToAgent::Deploy {
                project,
                deployment,
                repo,
                credential,
                ..
            } => {
                let Some(runner) = self.runner(&project) else {
                    self.progress(
                        &project,
                        &deployment,
                        Status::Failed,
                        None,
                        Some("this host does not have that project".to_owned()),
                    );
                    return;
                };
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    let mut runner = runner.lock().await;
                    runner.stopped = false;
                    let source = pipeline::Source {
                        repo: repo.as_deref(),
                        credential: credential.as_ref(),
                    };
                    if let Err(error) =
                        pipeline::deploy(&agent, &mut runner, &deployment, source).await
                    {
                        say_err(format!("deployment failed: {error}"));
                        agent.progress(
                            &project,
                            &deployment,
                            Status::Failed,
                            None,
                            Some(error.to_string()),
                        );
                    }
                    agent.report_state(&runner).await;
                });
            }
            ToAgent::Rollback {
                project,
                deployment,
            } => {
                let Some(runner) = self.runner(&project) else {
                    self.note(format!(
                        "rollback failed: this host does not have {project}"
                    ));
                    return;
                };
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    let mut runner = runner.lock().await;
                    runner.stopped = false;
                    if let Err(error) = pipeline::rollback(&agent, &mut runner, &deployment).await {
                        agent.note(format!("rollback of {project} failed: {error}"));
                        agent.progress(
                            &project,
                            &deployment,
                            Status::Retired,
                            None,
                            Some(error.to_string()),
                        );
                    }
                    agent.report_state(&runner).await;
                });
            }
            ToAgent::Restart { project } => {
                let Some(runner) = self.runner(&project) else {
                    return;
                };
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    let mut runner = runner.lock().await;
                    runner.stopped = false;
                    match pipeline::restart(&agent, &mut runner).await {
                        Ok(()) => agent.note(format!("restarted {project}")),
                        Err(error) => agent.note(format!("restart of {project} failed: {error}")),
                    }
                    agent.report_state(&runner).await;
                });
            }
            ToAgent::Stop { project } => {
                let Some(runner) = self.runner(&project) else {
                    return;
                };
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    let mut runner = runner.lock().await;
                    runner.stopped = true;
                    if let Err(error) = pipeline::stop(&mut runner).await {
                        agent.note(format!("stop of {project} failed: {error}"));
                    }
                    agent.report_state(&runner).await;
                });
            }
            ToAgent::DryRun {
                project,
                request,
                repo,
                credential,
            } => {
                let Some(runner) = self.runner(&project) else {
                    self.send(ToHub::DryRun {
                        project,
                        request,
                        ok: false,
                        message: "this host does not have that project".to_owned(),
                    });
                    return;
                };
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    let mut runner = runner.lock().await;
                    let source = pipeline::Source {
                        repo: repo.as_deref(),
                        credential: credential.as_ref(),
                    };
                    let (ok, message) = match pipeline::dry_run(&agent, &mut runner, source).await {
                        Ok(message) => (true, message),
                        Err(error) => (false, format!("dry run failed: {error}")),
                    };
                    agent.note(message.clone());
                    agent.send(ToHub::DryRun {
                        project,
                        request,
                        ok,
                        message,
                    });
                });
            }
            ToAgent::Ping => self.send(ToHub::Pong),
        }
    }

    /// Watch for cartons that exit on their own and bring them back with a
    /// growing delay, unless they were stopped on purpose.
    pub async fn monitor(self: Arc<Self>) {
        loop {
            tokio::time::sleep(MONITOR_INTERVAL).await;
            let runners: Vec<_> = self
                .projects
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned()
                .collect();
            for runner in runners {
                let Ok(mut runner) = runner.try_lock() else {
                    continue;
                };
                let Some(carton) = runner.running.as_mut() else {
                    continue;
                };
                let exit = match carton.exited().await {
                    Ok(Some(exit)) => exit,
                    _ => continue,
                };
                let id = runner.project.id.clone();
                runner.running = None;
                self.note(format!("{id}: {exit}"));
                if runner.stopped {
                    continue;
                }
                let delay = Duration::from_secs(
                    5u64.saturating_mul(2u64.pow(runner.restarts.min(4)))
                        .min(60),
                );
                runner.restarts += 1;
                let attempt = runner.restarts;
                self.note(format!(
                    "{id}: restarting in {}s (attempt {attempt})",
                    delay.as_secs()
                ));
                self.report_state(&runner).await;
                let agent = Arc::clone(&self);
                let runner_ref = self.runner(&id);
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let Some(runner) = runner_ref else {
                        return;
                    };
                    let mut runner = runner.lock().await;
                    if runner.stopped || runner.running.is_some() {
                        return;
                    }
                    if let Err(error) = pipeline::resume(&agent, &mut runner).await {
                        agent.note(format!("{id}: restart failed: {error}"));
                    }
                    agent.report_state(&runner).await;
                });
            }
        }
    }

    /// Let go of everything this host runs; called when the agent shuts
    /// down. Processes stop with it; containers keep running and are
    /// adopted next time.
    pub async fn shutdown(&self) {
        let runners: Vec<_> = self
            .projects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let mut left = Vec::new();
        for runner in runners {
            let mut runner = runner.lock().await;
            if let Some(tail) = runner.tail.take() {
                let _ = tail.send(true);
            }
            if let Some(carton) = runner.running.take() {
                match carton.release().await {
                    Ok(Some(container)) => {
                        left.push(format!("{} ({container})", runner.project.id))
                    }
                    Ok(None) => {}
                    Err(error) => say_err(format!("failed to stop {}: {error}", runner.project.id)),
                }
            }
        }
        if !left.is_empty() {
            say(format!(
                "containers keep running: {}; stop them with tinycd stop <project>",
                left.join(", ")
            ));
        }
    }
}

/// Follow a log file as it grows and send each line to the hub.
async fn tail(
    path: PathBuf,
    project: String,
    deployment: String,
    outbox: mpsc::UnboundedSender<ToHub>,
    mut stop: watch::Receiver<bool>,
    from_start: bool,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut position = if from_start {
        0
    } else {
        tokio::fs::metadata(&path)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let mut partial = Vec::new();
    loop {
        if let Ok(mut file) = tokio::fs::File::open(&path).await
            && file.seek(std::io::SeekFrom::Start(position)).await.is_ok()
        {
            let mut bytes = Vec::new();
            if file.read_to_end(&mut bytes).await.is_ok() && !bytes.is_empty() {
                position += bytes.len() as u64;
                partial.extend_from_slice(&bytes);
                while let Some(end) = partial.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = partial.drain(..=end).collect();
                    let text = String::from_utf8_lossy(&line).trim_end().to_owned();
                    let _ = outbox.send(ToHub::Log {
                        project: project.clone(),
                        deployment: deployment.clone(),
                        line: text,
                    });
                }
            }
        }
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
            _ = stop.changed() => {}
        }
    }
}
