//! The hub: the control plane behind the dashboard. It keeps the records,
//! decides what to deploy and when, and hands the work to agents. One
//! agent usually lives in the hub's own process, so a single machine needs
//! nothing else.

pub mod github;
pub mod poll;
pub mod server;
pub mod tls;
pub mod web;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::SocketAddr,
    sync::{
        Arc, Mutex as SyncMutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::{
    agent::{Agent, link},
    config,
    model::{
        Deployment, Event, Host, HostView, JoinToken, LOCAL_HOST, Overview, Project, ProjectView,
        RuntimeKind, Settings, Status,
    },
    protocol::{GitCredential, ToAgent, ToHub},
    store::{DaemonInfo, Store},
    util::{self, Error, say, say_err},
};

/// How often agents are pinged, and after how long without an answer they
/// count as gone.
const PING_INTERVAL: Duration = Duration::from_secs(30);
const AGENT_TIMEOUT: u64 = 90_000;

/// How long a reconnected agent gets to re-report deployments that were in
/// flight before they count as lost.
const LOST_AFTER: Duration = Duration::from_secs(45);

/// Join tokens expire after a day.
const JOIN_TOKEN_LIFETIME: u64 = 24 * 3600 * 1000;

pub struct HubOptions {
    pub bind: SocketAddr,
    pub secure_bind: Option<SocketAddr>,
    pub local_agent: bool,
    /// Print activity lines to stdout as well as broadcasting them.
    pub echo: bool,
}

pub struct AgentHandle {
    pub tx: mpsc::UnboundedSender<ToAgent>,
    /// Distinguishes this connection from a later one by the same host.
    pub connection: u64,
    pub last_seen: u64,
}

#[derive(Default)]
pub struct State {
    pub settings: Settings,
    pub hosts: Vec<Host>,
    pub projects: BTreeMap<String, Project>,
    pub deployments: BTreeMap<String, Vec<Deployment>>,
    pub agents: HashMap<String, AgentHandle>,
    /// The deployment each host is working on, per project.
    pub inflight: HashMap<String, String>,
    /// The deployment to run next, per project; newer triggers replace it.
    pub pending: HashMap<String, String>,
    pub polling: HashSet<String>,
}

struct DryRunWaiter {
    project: String,
    reply: oneshot::Sender<(bool, String)>,
}

pub struct Hub {
    me: Weak<Hub>,
    pub store: Store,
    pub state: Mutex<State>,
    pub events: broadcast::Sender<Event>,
    pub http: reqwest::Client,
    pub api_token: String,
    pub started_at: u64,
    /// Where the plain HTTP listener is reached from this machine.
    pub url: String,
    pub bind: SocketAddr,
    pub tls: tls::TlsIdentity,
    sessions: SyncMutex<HashSet<String>>,
    github_state: SyncMutex<Option<(String, u64)>>,
    github_tokens: Mutex<HashMap<u64, (String, u64)>>,
    installations: Mutex<HashMap<String, (Option<u64>, u64)>>,
    dry_runs: SyncMutex<HashMap<u64, DryRunWaiter>>,
    next_request: AtomicU64,
    local_agent: SyncMutex<Option<Arc<Agent>>>,
    secure_started: AtomicBool,
    options_secure_bind: Option<SocketAddr>,
    echo: bool,
}

impl Hub {
    pub async fn open(store: Store, options: &HubOptions) -> Result<Arc<Self>, Error> {
        let api_token = store.api_token()?;
        let mut settings = store.settings()?;
        if let Some(secure) = options.secure_bind {
            settings.secure_bind = Some(secure.to_string());
        }
        let mut hosts = store.hosts()?;
        if !hosts.iter().any(|host| host.id == LOCAL_HOST) {
            hosts.insert(
                0,
                Host {
                    id: LOCAL_HOST.to_owned(),
                    name: util::hostname(),
                    local: true,
                    created_at: util::now_ms(),
                    ..Host::default()
                },
            );
            store.save_hosts(&hosts)?;
        }
        let mut projects = BTreeMap::new();
        let mut deployments = BTreeMap::new();
        for project in store.projects()? {
            deployments.insert(project.id.clone(), store.deployments(&project.id)?);
            projects.insert(project.id.clone(), project);
        }

        let names: Vec<String> = settings
            .public_url
            .iter()
            .filter_map(|url| url.split("://").nth(1))
            .map(|rest| rest.split(['/', ':']).next().unwrap_or(rest).to_owned())
            .collect();
        let identity = tls::ensure(&store.tls_dir(), &names)?;
        let sessions = store.sessions().into_iter().collect();
        let http = reqwest::Client::builder()
            .user_agent(format!("tinycd/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?;

        let (events, _) = broadcast::channel(1024);
        let url = format!("http://{}", options.bind);
        Ok(Arc::new_cyclic(|me| Hub {
            me: me.clone(),
            store,
            state: Mutex::new(State {
                settings,
                hosts,
                projects,
                deployments,
                ..State::default()
            }),
            events,
            http,
            api_token,
            started_at: util::now_ms(),
            url,
            bind: options.bind,
            tls: identity,
            sessions: SyncMutex::new(sessions),
            github_state: SyncMutex::new(None),
            github_tokens: Mutex::new(HashMap::new()),
            installations: Mutex::new(HashMap::new()),
            dry_runs: SyncMutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            local_agent: SyncMutex::new(None),
            secure_started: AtomicBool::new(false),
            options_secure_bind: options.secure_bind,
            echo: options.echo,
        }))
    }

    fn arc(&self) -> Arc<Hub> {
        self.me.upgrade().expect("the hub outlives its tasks")
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub fn note(&self, text: impl Into<String>) {
        let text = text.into();
        if self.echo {
            println!("{text}");
        }
        self.emit(Event::Message { text });
    }

    /// The URL other machines and GitHub use, when one is configured.
    pub async fn public_url(&self) -> String {
        self.state
            .lock()
            .await
            .settings
            .public_url
            .clone()
            .unwrap_or_else(|| self.url.clone())
    }

    // ----- running -------------------------------------------------------

    /// Serve until `shutdown` resolves, then stop everything the local
    /// agent runs.
    pub async fn run(
        self: &Arc<Self>,
        options: &HubOptions,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), Error> {
        let (activity_tx, mut activity_rx) = mpsc::unbounded_channel::<String>();
        util::set_activity_sink(activity_tx);
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(line) = activity_rx.recv().await {
                hub.note(line);
            }
        });

        let listener = tokio::net::TcpListener::bind(self.bind)
            .await
            .map_err(|error| format!("failed to listen on {}: {error}", self.bind))?;
        self.store.save_daemon(&DaemonInfo {
            url: self.url.clone(),
            pid: std::process::id(),
        })?;
        say(format!("dashboard at {}", self.url));
        let hub = Arc::clone(self);
        let http_server = tokio::spawn(async move {
            axum::serve(listener, server::router(hub))
                .await
                .map_err(|error| error.to_string())
        });
        self.ensure_secure_listener().await;

        if options.local_agent {
            self.start_local_agent().await;
        }
        tokio::spawn(poll::run(Arc::clone(self)));
        tokio::spawn(Arc::clone(self).pinger());

        tokio::select! {
            result = http_server => {
                self.store.clear_daemon();
                match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.into()),
                    Err(error) => Err(error.into()),
                }
            }
            () = shutdown => {
                say("shutting down");
                let agent = self.local_agent.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(agent) = agent {
                    agent.shutdown().await;
                }
                self.store.clear_daemon();
                Ok(())
            }
        }
    }

    async fn start_local_agent(self: &Arc<Self>) {
        let (to_agent_tx, to_agent_rx) = mpsc::unbounded_channel();
        let (to_hub_tx, mut to_hub_rx) = mpsc::unbounded_channel();
        let agent = Arc::new(Agent::new(self.store.agent_dir(), false, to_hub_tx).await);
        *self.local_agent.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&agent));
        tokio::spawn(link::run_local(Arc::clone(&agent), to_agent_rx));
        tokio::spawn(Arc::clone(&agent).monitor());
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(message) = to_hub_rx.recv().await {
                hub.handle_agent(LOCAL_HOST, message).await;
            }
        });
        let name = util::hostname();
        self.handle_agent(LOCAL_HOST, agent.register_message(&name))
            .await;
        self.attach_agent(LOCAL_HOST, to_agent_tx, None).await;
    }

    /// Start the HTTPS listener agents and remote browsers use, once, if
    /// one is configured.
    pub async fn ensure_secure_listener(self: &Arc<Self>) {
        let bind = {
            let state = self.state.lock().await;
            state.settings.secure_bind.clone()
        };
        let Some(bind) = bind else {
            return;
        };
        if self.secure_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let address: SocketAddr = match bind.parse() {
            Ok(address) => address,
            Err(error) => {
                say_err(format!("ignoring secure_bind {bind:?}: {error}"));
                return;
            }
        };
        util::init_crypto();
        let config = match axum_server::tls_rustls::RustlsConfig::from_pem(
            self.tls.cert_pem.clone().into_bytes(),
            self.tls.key_pem.clone().into_bytes(),
        )
        .await
        {
            Ok(config) => config,
            Err(error) => {
                say_err(format!("failed to load the TLS certificate: {error}"));
                return;
            }
        };
        let router = server::router(Arc::clone(self));
        say(format!(
            "secure listener on https://{address} (fingerprint {})",
            &self.tls.fingerprint[..16]
        ));
        tokio::spawn(async move {
            if let Err(error) = axum_server::bind_rustls(address, config)
                .serve(router.into_make_service())
                .await
            {
                say_err(format!("secure listener failed: {error}"));
            }
        });
    }

    async fn pinger(self: Arc<Self>) {
        loop {
            tokio::time::sleep(PING_INTERVAL).await;
            let now = util::now_ms();
            let mut gone = Vec::new();
            {
                let state = self.state.lock().await;
                for (host, handle) in &state.agents {
                    if host == LOCAL_HOST {
                        continue;
                    }
                    if now.saturating_sub(handle.last_seen) > AGENT_TIMEOUT {
                        gone.push(host.clone());
                    } else {
                        let _ = handle.tx.send(ToAgent::Ping);
                    }
                }
            }
            for host in gone {
                self.note(format!("host {host} stopped answering"));
                self.detach_agent(&host, None).await;
            }
        }
    }

    // ----- agents --------------------------------------------------------

    /// Register a connected agent and tell it what it runs. Returns the
    /// connection id to detach with.
    pub async fn attach_agent(
        &self,
        host_id: &str,
        tx: mpsc::UnboundedSender<ToAgent>,
        token: Option<String>,
    ) -> u64 {
        let now = util::now_ms();
        let connection = self.next_request.fetch_add(1, Ordering::SeqCst);
        let projects: Vec<Project> = {
            let mut state = self.state.lock().await;
            state.agents.insert(
                host_id.to_owned(),
                AgentHandle {
                    tx: tx.clone(),
                    connection,
                    last_seen: now,
                },
            );
            if let Some(host) = state.hosts.iter_mut().find(|host| host.id == host_id) {
                host.last_seen = Some(now);
            }
            let _ = self.store.save_hosts(&state.hosts);
            state
                .projects
                .values()
                .filter(|project| project.host == host_id)
                .cloned()
                .collect()
        };
        self.emit(Event::Host {
            host: host_id.to_owned(),
            online: true,
        });
        if host_id != LOCAL_HOST {
            self.note(format!("host {host_id} connected"));
        }
        let _ = tx.send(ToAgent::Welcome {
            host: host_id.to_owned(),
            projects,
            token,
        });

        // Deployments that were in flight on this host must be re-reported
        // soon, or they were lost with the previous connection.
        let hub = self.arc();
        let host = host_id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(LOST_AFTER).await;
            let mut state = hub.state.lock().await;
            let stale: Vec<(String, String)> = state
                .inflight
                .iter()
                .filter(|(project, _)| state.projects.get(*project).is_some_and(|p| p.host == host))
                .map(|(project, deployment)| (project.clone(), deployment.clone()))
                .collect();
            for (project, id) in stale {
                let lost = state
                    .deployments
                    .get(&project)
                    .and_then(|list| list.iter().find(|d| d.id == id))
                    .is_some_and(|d| d.status.in_progress() && d.updated_at < now);
                if lost {
                    let mut deployment = state
                        .deployments
                        .get(&project)
                        .and_then(|list| list.iter().find(|d| d.id == id))
                        .cloned()
                        .expect("checked above");
                    deployment.status = Status::Failed;
                    deployment.error =
                        Some("lost: the host reconnected without reporting on it".to_owned());
                    deployment.updated_at = util::now_ms();
                    hub.save_deployment(&mut state, deployment);
                    hub.finish_inflight(&mut state, &project);
                }
            }
        });
        connection
    }

    /// Forget a host's connection, unless a newer one replaced it.
    pub async fn detach_agent(&self, host_id: &str, connection: Option<u64>) {
        let removed = {
            let mut state = self.state.lock().await;
            let ours = state
                .agents
                .get(host_id)
                .is_some_and(|handle| connection.is_none_or(|c| handle.connection == c));
            let removed = ours && state.agents.remove(host_id).is_some();
            if let Some(host) = state.hosts.iter_mut().find(|host| host.id == host_id) {
                host.last_seen = Some(util::now_ms());
            }
            let _ = self.store.save_hosts(&state.hosts);
            removed
        };
        if removed {
            self.emit(Event::Host {
                host: host_id.to_owned(),
                online: false,
            });
            self.note(format!("host {host_id} disconnected"));
        }
    }

    /// Resolve a bearer token presented on the agent socket: a host's own
    /// token, or a join token that creates the host. Returns the host id
    /// and, for a new host, its permanent token.
    pub async fn authenticate_agent(&self, bearer: &str) -> Option<(String, Option<String>)> {
        let mut state = self.state.lock().await;
        if let Some(host) = state.hosts.iter().find(|host| {
            !host.local
                && !host.token_hash.is_empty()
                && util::token_matches(bearer, &host.token_hash)
        }) {
            return Some((host.id.clone(), None));
        }

        let now = util::now_ms();
        state
            .settings
            .join_tokens
            .retain(|join| join.expires_at > now);
        let index = state
            .settings
            .join_tokens
            .iter()
            .position(|join| util::token_matches(bearer, &join.secret_hash))?;
        let join = state.settings.join_tokens.remove(index);
        let _ = self.store.save_settings(&state.settings);

        let base = util::slug(&join.name);
        let mut id = base.clone();
        let mut counter = 2;
        while id == LOCAL_HOST || state.hosts.iter().any(|host| host.id == id) {
            id = format!("{base}-{counter}");
            counter += 1;
        }
        let token = util::random_hex(32);
        state.hosts.push(Host {
            id: id.clone(),
            name: join.name,
            token_hash: util::token_hash(&token),
            created_at: now,
            ..Host::default()
        });
        let _ = self.store.save_hosts(&state.hosts);
        drop(state);
        self.note(format!("host {id} joined"));
        Some((id, Some(token)))
    }

    /// Mint a one-time join token for a new host and make sure the secure
    /// listener it will use is up.
    pub async fn create_join_token(
        self: &Arc<Self>,
        name: &str,
    ) -> Result<(String, String), Error> {
        let name = name.trim();
        if name.is_empty() {
            return Err("the host needs a name".into());
        }
        let secret = util::random_hex(32);
        let url = {
            let mut state = self.state.lock().await;
            if state.settings.secure_bind.is_none() {
                state.settings.secure_bind = Some(
                    self.options_secure_bind
                        .map(|address| address.to_string())
                        .unwrap_or_else(|| "0.0.0.0:4443".to_owned()),
                );
            }
            state.settings.join_tokens.push(JoinToken {
                name: name.to_owned(),
                secret_hash: util::token_hash(&secret),
                expires_at: util::now_ms() + JOIN_TOKEN_LIFETIME,
            });
            self.store.save_settings(&state.settings)?;
            let port = state
                .settings
                .secure_bind
                .as_deref()
                .and_then(|bind| bind.rsplit(':').next())
                .unwrap_or("4443")
                .to_owned();
            // The join token pins this listener, not a reverse proxy's
            // certificate at the public dashboard URL.
            format!("https://{}:{port}", reachable_address())
        };
        self.ensure_secure_listener().await;
        Ok((format!("tcj1.{}.{secret}", self.tls.fingerprint), url))
    }

    /// Act on a message from an agent.
    pub async fn handle_agent(&self, host_id: &str, message: ToHub) {
        let mut state = self.state.lock().await;
        if let Some(handle) = state.agents.get_mut(host_id) {
            handle.last_seen = util::now_ms();
        }
        let project = match &message {
            ToHub::Progress { project, .. }
            | ToHub::Log { project, .. }
            | ToHub::State { project, .. }
            | ToHub::DryRun { project, .. } => Some(project),
            _ => None,
        };
        if let Some(project) = project
            && !state
                .projects
                .get(project)
                .is_some_and(|p| p.host == host_id)
        {
            return;
        }
        // Agent-controlled release ids become filenames when adopting state.
        // Logs may only name releases the hub already knows.
        match &message {
            ToHub::State {
                current, retained, ..
            } if current.iter().chain(retained).any(|id| {
                id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }) =>
            {
                return;
            }
            ToHub::Log {
                project,
                deployment,
                ..
            } if !state
                .deployments
                .get(project)
                .is_some_and(|list| list.iter().any(|d| d.id == *deployment)) =>
            {
                return;
            }
            _ => {}
        }
        match message {
            ToHub::Register {
                name,
                os,
                version,
                runtimes,
            } => {
                if let Some(host) = state.hosts.iter_mut().find(|host| host.id == host_id) {
                    if host.name.is_empty() {
                        host.name = name;
                    }
                    host.os = os;
                    host.version = version;
                    host.runtimes = runtimes;
                    host.last_seen = Some(util::now_ms());
                }
                let _ = self.store.save_hosts(&state.hosts);
            }
            ToHub::Progress {
                project,
                deployment,
                status,
                commit,
                error,
            } => {
                let Some(mut record) = state
                    .deployments
                    .get(&project)
                    .and_then(|list| list.iter().find(|d| d.id == deployment))
                    .cloned()
                else {
                    return;
                };
                if !record.status.in_progress()
                    && status.in_progress()
                    && record.status != Status::Retired
                {
                    // A rollback or a re-report of something already
                    // settled must not resurrect it.
                    if status != Status::Starting && status != Status::Checking {
                        return;
                    }
                }
                record.status = status;
                record.updated_at = util::now_ms();
                if let Some(commit) = &commit {
                    record.commit = commit.clone();
                }
                record.error = error;
                if status == Status::Live {
                    record.retained = true;
                    let others: Vec<Deployment> = state
                        .deployments
                        .get(&project)
                        .map(|list| {
                            list.iter()
                                .filter(|d| d.id != deployment && d.status == Status::Live)
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    for mut other in others {
                        other.status = Status::Retired;
                        other.updated_at = util::now_ms();
                        self.save_deployment(&mut state, other);
                    }
                    if let Some(p) = state.projects.get_mut(&project) {
                        p.current = Some(deployment.clone());
                        p.running = true;
                        if let Some(commit) = &commit {
                            p.head = Some(commit.clone());
                        }
                        let p = p.clone();
                        self.save_project(&mut state, p);
                    }
                } else if let Some(commit) = &commit
                    && let Some(p) = state.projects.get_mut(&project)
                {
                    p.head = Some(commit.clone());
                    let p = p.clone();
                    self.save_project(&mut state, p);
                }
                if status == Status::Failed {
                    record.retained = false;
                }
                self.save_deployment(&mut state, record.clone());
                if !status.in_progress() {
                    if state.inflight.get(&project) == Some(&deployment) {
                        self.finish_inflight(&mut state, &project);
                    }
                    if let Some(error) = &record.error {
                        self.note(format!(
                            "{project}: {} {}: {error}",
                            record.id,
                            status.label()
                        ));
                    }
                }
                if let Some(p) = state.projects.get(&project).cloned() {
                    github::report_status(self.arc(), p, record);
                }
            }
            ToHub::Log {
                project,
                deployment,
                line,
            } => {
                self.store.append_log(&project, &deployment, &line);
                self.emit(Event::Log {
                    project,
                    deployment,
                    line,
                });
            }
            ToHub::State {
                project,
                current,
                running,
                retained,
            } => {
                let Some(mut p) = state.projects.get(&project).cloned() else {
                    return;
                };
                let mut list = state.deployments.get(&project).cloned().unwrap_or_default();
                for id in &retained {
                    if !list.iter().any(|d| &d.id == id) {
                        let adopted = Deployment {
                            id: id.clone(),
                            project: project.clone(),
                            trigger: "adopted".to_owned(),
                            status: Status::Retired,
                            created_at: id.parse::<u64>().unwrap_or_else(|_| util::now_ms()),
                            updated_at: util::now_ms(),
                            ..Deployment::default()
                        };
                        list.push(adopted);
                    }
                }
                list.sort_by(|a, b| a.id.cmp(&b.id));
                for record in list.iter_mut() {
                    let on_disk = retained.contains(&record.id);
                    let mut changed = false;
                    if record.retained != on_disk && !record.status.in_progress() {
                        record.retained = on_disk;
                        changed = true;
                    }
                    let is_current = current.as_deref() == Some(record.id.as_str());
                    if is_current && record.status != Status::Live && !record.status.in_progress() {
                        record.status = Status::Live;
                        changed = true;
                    } else if !is_current && record.status == Status::Live {
                        record.status = Status::Retired;
                        changed = true;
                    }
                    if changed {
                        record.updated_at = util::now_ms();
                        let _ = self.store.save_deployment(record);
                        self.emit(Event::Deployment {
                            deployment: record.clone(),
                        });
                    }
                }
                state.deployments.insert(project.clone(), list);
                if p.current != current || p.running != running {
                    p.current = current;
                    p.running = running;
                    self.save_project(&mut state, p);
                }
            }
            ToHub::DryRun {
                project,
                request,
                ok,
                message,
            } => {
                let mut waiters = self.dry_runs.lock().unwrap_or_else(|e| e.into_inner());
                if waiters
                    .get(&request)
                    .is_some_and(|waiter| waiter.project == project)
                    && let Some(waiter) = waiters.remove(&request)
                {
                    let _ = waiter.reply.send((ok, message));
                }
            }
            ToHub::Note { text } => {
                let name = state
                    .hosts
                    .iter()
                    .find(|host| host.id == host_id)
                    .map(|host| host.name.clone())
                    .unwrap_or_else(|| host_id.to_owned());
                self.note(format!("[{name}] {text}"));
            }
            ToHub::Pong => {}
        }
    }

    fn save_deployment(&self, state: &mut State, deployment: Deployment) {
        let _ = self.store.save_deployment(&deployment);
        let list = state
            .deployments
            .entry(deployment.project.clone())
            .or_default();
        match list.iter_mut().find(|d| d.id == deployment.id) {
            Some(existing) => *existing = deployment.clone(),
            None => {
                list.push(deployment.clone());
                list.sort_by(|a, b| a.id.cmp(&b.id));
            }
        }
        self.emit(Event::Deployment { deployment });
    }

    fn save_project(&self, state: &mut State, project: Project) {
        let _ = self.store.save_project(&project);
        state.projects.insert(project.id.clone(), project.clone());
        self.emit(Event::Project { project });
    }

    /// The host finished with a project's deployment; start the next one
    /// if a newer trigger queued up meanwhile.
    fn finish_inflight(&self, state: &mut State, project: &str) {
        state.inflight.remove(project);
        if let Some(next) = state.pending.remove(project) {
            state.inflight.insert(project.to_owned(), next.clone());
            let hub = self.arc();
            let project = project.to_owned();
            tokio::spawn(async move { hub.dispatch(&project, &next).await });
        }
    }

    // ----- deployments ---------------------------------------------------

    /// Queue a deployment. If one is in flight, this replaces whatever was
    /// waiting behind it, so a burst of pushes yields one deployment of
    /// the newest commit.
    pub async fn trigger(
        &self,
        project_id: &str,
        trigger: &str,
        commit: Option<String>,
    ) -> Result<String, Error> {
        let (id, dispatch_now) = {
            let mut state = self.state.lock().await;
            let project = state
                .projects
                .get(project_id)
                .cloned()
                .ok_or_else(|| format!("no project named {project_id}"))?;
            if !state.agents.contains_key(&project.host) {
                return Err(format!("host {} is offline", project.host).into());
            }
            let mut id = util::now_ms();
            while state
                .deployments
                .get(project_id)
                .is_some_and(|list| list.iter().any(|d| d.id == id.to_string()))
            {
                id += 1;
            }
            let id = id.to_string();
            let deployment = Deployment {
                id: id.clone(),
                project: project_id.to_owned(),
                commit: commit.clone().unwrap_or_default(),
                trigger: trigger.to_owned(),
                status: Status::Queued,
                created_at: util::now_ms(),
                updated_at: util::now_ms(),
                ..Deployment::default()
            };
            self.save_deployment(&mut state, deployment);
            if let Some(commit) = commit
                && let Some(p) = state.projects.get_mut(project_id)
            {
                p.head = Some(commit);
                let p = p.clone();
                self.save_project(&mut state, p);
            }

            let dispatch_now = !state.inflight.contains_key(project_id);
            if dispatch_now {
                state.inflight.insert(project_id.to_owned(), id.clone());
            } else {
                if let Some(previous) = state.pending.insert(project_id.to_owned(), id.clone())
                    && let Some(mut cancelled) = state
                        .deployments
                        .get(project_id)
                        .and_then(|list| list.iter().find(|d| d.id == previous))
                        .cloned()
                {
                    cancelled.status = Status::Cancelled;
                    cancelled.retained = false;
                    cancelled.updated_at = util::now_ms();
                    cancelled.error = Some("superseded by a newer trigger".to_owned());
                    self.save_deployment(&mut state, cancelled);
                }
                self.note(format!(
                    "{project_id}: queued {id} behind the running deployment"
                ));
            }
            (id, dispatch_now)
        };
        if dispatch_now {
            self.dispatch(project_id, &id).await;
        }
        Ok(id)
    }

    /// Send a queued deployment to its host.
    async fn dispatch(&self, project_id: &str, deployment: &str) {
        let Some(project) = self.state.lock().await.projects.get(project_id).cloned() else {
            return;
        };
        let (repo, credential) = self.clone_source(&project).await;
        let mut state = self.state.lock().await;
        let Some(handle) = state.agents.get(&project.host) else {
            let mut record = match state
                .deployments
                .get(project_id)
                .and_then(|list| list.iter().find(|d| d.id == deployment))
                .cloned()
            {
                Some(record) => record,
                None => return,
            };
            record.status = Status::Failed;
            record.retained = false;
            record.error = Some(format!("host {} is offline", project.host));
            record.updated_at = util::now_ms();
            self.save_deployment(&mut state, record);
            self.finish_inflight(&mut state, project_id);
            return;
        };
        let _ = handle.tx.send(ToAgent::Deploy {
            project: project_id.to_owned(),
            deployment: deployment.to_owned(),
            commit: None,
            repo: (repo != project.repo).then_some(repo),
            credential,
        });
        self.note(format!("{project_id}: deploying {deployment}"));
    }

    /// The URL to clone and the credential for it: the project's own, or
    /// GitHub over HTTPS with an installation token when the App covers
    /// the repository.
    pub async fn clone_source(&self, project: &Project) -> (String, Option<GitCredential>) {
        match github::credential_for(self, project).await {
            Some((url, credential)) => (url, Some(credential)),
            None => (project.repo.clone(), None),
        }
    }

    fn send_to_host(&self, state: &State, host: &str, message: ToAgent) -> Result<(), Error> {
        let handle = state
            .agents
            .get(host)
            .ok_or_else(|| format!("host {host} is offline"))?;
        handle
            .tx
            .send(message)
            .map_err(|_| format!("host {host} is disconnecting").into())
    }

    /// Start a retained deployment again. Without a target, the retained
    /// deployment before the current one is used.
    pub async fn rollback(&self, project_id: &str, target: Option<&str>) -> Result<String, Error> {
        let state = self.state.lock().await;
        let project = state
            .projects
            .get(project_id)
            .ok_or_else(|| format!("no project named {project_id}"))?;
        let list = state
            .deployments
            .get(project_id)
            .cloned()
            .unwrap_or_default();
        let candidates: Vec<&Deployment> = list
            .iter()
            .filter(|d| d.retained && matches!(d.status, Status::Live | Status::Retired))
            .collect();
        let target = match target.filter(|t| !t.is_empty()) {
            Some(target) => candidates
                .iter()
                .find(|d| d.id == target)
                .map(|d| d.id.clone())
                .ok_or_else(|| {
                    format!(
                        "release {target} is not retained; retained: {}",
                        candidates
                            .iter()
                            .map(|d| d.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?,
            None => {
                let current = project.current.clone().unwrap_or_default();
                candidates
                    .iter()
                    .rev()
                    .find(|d| d.id < current)
                    .map(|d| d.id.clone())
                    .ok_or("no release older than the current one is retained")?
            }
        };
        self.send_to_host(
            &state,
            &project.host,
            ToAgent::Rollback {
                project: project_id.to_owned(),
                deployment: target.clone(),
            },
        )?;
        self.note(format!("{project_id}: rolling back to {target}"));
        Ok(target)
    }

    pub async fn restart(&self, project_id: &str) -> Result<(), Error> {
        let state = self.state.lock().await;
        let project = state
            .projects
            .get(project_id)
            .ok_or_else(|| format!("no project named {project_id}"))?;
        self.send_to_host(
            &state,
            &project.host,
            ToAgent::Restart {
                project: project_id.to_owned(),
            },
        )
    }

    pub async fn stop(&self, project_id: &str) -> Result<(), Error> {
        let state = self.state.lock().await;
        let project = state
            .projects
            .get(project_id)
            .ok_or_else(|| format!("no project named {project_id}"))?;
        self.send_to_host(
            &state,
            &project.host,
            ToAgent::Stop {
                project: project_id.to_owned(),
            },
        )
    }

    /// Build and verify one throwaway release on the project's host and
    /// wait for the verdict.
    pub async fn dry_run(&self, project_id: &str) -> Result<String, Error> {
        let project = self
            .state
            .lock()
            .await
            .projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| format!("no project named {project_id}"))?;
        let (repo, credential) = self.clone_source(&project).await;
        let request = self.next_request.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.dry_runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                request,
                DryRunWaiter {
                    project: project_id.to_owned(),
                    reply: tx,
                },
            );
        {
            let state = self.state.lock().await;
            self.send_to_host(
                &state,
                &project.host,
                ToAgent::DryRun {
                    project: project_id.to_owned(),
                    request,
                    repo: (repo != project.repo).then_some(repo),
                    credential,
                },
            )?;
        }
        match tokio::time::timeout(Duration::from_secs(20 * 60), rx).await {
            Ok(Ok((true, message))) => Ok(message),
            Ok(Ok((false, message))) => Err(message.into()),
            Ok(Err(_)) => Err("the host went away during the dry run".into()),
            Err(_) => {
                self.dry_runs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request);
                Err("the dry run did not finish within 20 minutes".into())
            }
        }
    }

    // ----- projects ------------------------------------------------------

    /// Create or update a project and tell its host.
    pub async fn upsert_project(&self, mut project: Project) -> Result<Project, Error> {
        project.id = util::slug(&project.id);
        if project.name.trim().is_empty() {
            project.name = project.id.clone();
        }
        if project.repo.trim().is_empty() {
            return Err("the project needs a repository URL".into());
        }
        if project.keep == 0 {
            return Err("keep must be at least one deployment".into());
        }
        if project.poll == Some(0) {
            return Err("poll must be at least one second".into());
        }
        if project
            .git_ref
            .as_deref()
            .is_some_and(|r| r.trim().is_empty())
        {
            project.git_ref = None;
        }
        config::validate_share(&mut project.share)?;
        if let Some(token) = &project.webhook_token
            && (token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(
                "the webhook token must be at least 32 printable non-space ASCII bytes".into(),
            );
        }
        if project
            .recipe
            .shell
            .as_deref()
            .is_some_and(|shell| shell.is_empty())
        {
            return Err("shell must name a program".into());
        }

        let mut state = self.state.lock().await;
        let host = state
            .hosts
            .iter()
            .find(|host| host.id == project.host)
            .ok_or_else(|| format!("no host named {}", project.host))?;
        if !host.runtimes.is_empty() && !host.runtimes.contains(&project.runtime) {
            return Err(format!(
                "host {} has no {} runtime",
                host.id,
                project.runtime.label()
            )
            .into());
        }
        if project.runtime == RuntimeKind::Oci && project.recipe.port.is_none() {
            say(format!(
                "{}: no port set; the container will not publish one",
                project.id
            ));
        }

        let previous = state.projects.get(&project.id).cloned();
        match &previous {
            Some(previous) => {
                if previous.root != project.root {
                    return Err("a project's release root cannot be changed; create a new project for a different root".into());
                }
                if previous.runtime != project.runtime && previous.current.is_some() {
                    return Err("a project's runtime cannot be changed after deployment; create a new project for a different runtime".into());
                }
                project.created_at = previous.created_at;
                project.current = previous.current.clone();
                project.running = previous.running;
                project.head = previous.head.clone();
                if previous.host != project.host {
                    let _ = self.send_to_host(
                        &state,
                        &previous.host,
                        ToAgent::Unassign {
                            project: project.id.clone(),
                            stop: true,
                        },
                    );
                    project.current = None;
                    project.running = false;
                }
            }
            None => {
                project.created_at = util::now_ms();
                state.deployments.entry(project.id.clone()).or_default();
            }
        }
        self.save_project(&mut state, project.clone());
        let _ = self.send_to_host(
            &state,
            &project.host,
            ToAgent::Assign {
                project: project.clone(),
            },
        );
        Ok(project)
    }

    pub async fn remove_project(&self, project_id: &str, stop: bool) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let project = state
            .projects
            .remove(project_id)
            .ok_or_else(|| format!("no project named {project_id}"))?;
        state.deployments.remove(project_id);
        state.inflight.remove(project_id);
        state.pending.remove(project_id);
        let _ = self.send_to_host(
            &state,
            &project.host,
            ToAgent::Unassign {
                project: project_id.to_owned(),
                stop,
            },
        );
        self.store.delete_project(project_id)?;
        self.emit(Event::ProjectRemoved {
            project: project_id.to_owned(),
        });
        self.note(format!("removed project {project_id}"));
        Ok(())
    }

    pub async fn overview(&self) -> Overview {
        let state = self.state.lock().await;
        Overview {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at: self.started_at,
            url: self.url.clone(),
            hosts: state
                .hosts
                .iter()
                .map(|host| HostView {
                    host: host.clone(),
                    online: state.agents.contains_key(&host.id),
                })
                .collect(),
            projects: state
                .projects
                .values()
                .map(|project| project_view(&state, project))
                .collect(),
        }
    }

    pub async fn project_view(&self, project_id: &str) -> Option<ProjectView> {
        let state = self.state.lock().await;
        state
            .projects
            .get(project_id)
            .map(|project| project_view(&state, project))
    }

    pub async fn project(&self, project_id: &str) -> Option<Project> {
        self.state.lock().await.projects.get(project_id).cloned()
    }

    /// Read a deployment's log as the hub has it.
    pub async fn read_log(&self, project: &str, deployment: &str) -> String {
        tokio::fs::read_to_string(self.store.log_path(project, deployment))
            .await
            .unwrap_or_default()
    }

    // ----- settings and sessions ----------------------------------------

    pub async fn settings(&self) -> Settings {
        self.state.lock().await.settings.clone()
    }

    pub async fn update_settings(
        &self,
        apply: impl FnOnce(&mut Settings),
    ) -> Result<Settings, Error> {
        let mut state = self.state.lock().await;
        apply(&mut state.settings);
        self.store.save_settings(&state.settings)?;
        Ok(state.settings.clone())
    }

    pub async fn has_password(&self) -> bool {
        self.state.lock().await.settings.admin_password.is_some()
    }

    pub async fn set_password(&self, password: &str) -> Result<(), Error> {
        if password.len() < 8 {
            return Err("the password must be at least 8 characters".into());
        }
        let hash = util::hash_password(password);
        self.update_settings(|settings| settings.admin_password = Some(hash))
            .await?;
        Ok(())
    }

    pub async fn check_password(&self, password: &str) -> bool {
        let stored = self.state.lock().await.settings.admin_password.clone();
        stored.is_some_and(|stored| util::verify_password(password, &stored))
    }

    pub fn check_api_token(&self, presented: &str) -> bool {
        util::secret_eq(presented, &self.api_token)
    }

    pub fn new_session(&self) -> String {
        let id = util::random_hex(32);
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.insert(id.clone());
        let list: Vec<String> = sessions.iter().cloned().collect();
        self.store.save_sessions(&list);
        id
    }

    pub fn session_valid(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }

    pub fn end_session(&self, id: &str) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.remove(id);
        let list: Vec<String> = sessions.iter().cloned().collect();
        self.store.save_sessions(&list);
    }

    // ----- github plumbing ----------------------------------------------

    pub fn github_state(&self) -> String {
        let state = util::random_hex(16);
        *self.github_state.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((state.clone(), util::now_ms()));
        state
    }

    pub fn take_github_state(&self, presented: &str) -> bool {
        let mut slot = self.github_state.lock().unwrap_or_else(|e| e.into_inner());
        let Some((state, at)) = slot.as_ref() else {
            return false;
        };
        if util::now_ms().saturating_sub(*at) >= 15 * 60 * 1000 {
            *slot = None;
            return false;
        }
        if !util::secret_eq(state, presented) {
            return false;
        }
        *slot = None;
        true
    }

    pub async fn cached_installation_token(&self, installation: u64) -> Option<String> {
        let tokens = self.github_tokens.lock().await;
        tokens
            .get(&installation)
            .filter(|(_, expires)| *expires > util::now_ms() + 5 * 60 * 1000)
            .map(|(token, _)| token.clone())
    }

    pub async fn cache_installation_token(&self, installation: u64, token: String, expires: u64) {
        self.github_tokens
            .lock()
            .await
            .insert(installation, (token, expires));
    }

    pub async fn cached_installation(&self, repo: &str) -> Option<Option<u64>> {
        let cache = self.installations.lock().await;
        cache
            .get(repo)
            .filter(|(_, at)| util::now_ms().saturating_sub(*at) < 10 * 60 * 1000)
            .map(|(id, _)| *id)
    }

    pub async fn cache_installation(&self, repo: &str, id: Option<u64>) {
        self.installations
            .lock()
            .await
            .insert(repo.to_owned(), (id, util::now_ms()));
    }

    pub async fn forget_github(&self) {
        self.github_tokens.lock().await.clear();
        self.installations.lock().await.clear();
    }
}

fn project_view(state: &State, project: &Project) -> ProjectView {
    let mut deployments = state
        .deployments
        .get(&project.id)
        .cloned()
        .unwrap_or_default();
    deployments.reverse();
    deployments.truncate(50);
    ProjectView {
        project: project.clone(),
        deployments,
        in_progress: state.inflight.get(&project.id).cloned(),
    }
}

/// An address other machines on the network can probably reach this one
/// at: the interface that routes outward, else the hostname.
fn reachable_address() -> String {
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0")
        && socket.connect("192.0.2.1:9").is_ok()
        && let Ok(local) = socket.local_addr()
        && !local.ip().is_loopback()
        && !local.ip().is_unspecified()
    {
        return local.ip().to_string();
    }
    util::hostname()
}
