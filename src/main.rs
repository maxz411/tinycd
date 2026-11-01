//! tinycd: tiny deployments. One binary that is the dashboard, the host
//! that runs releases, and the command line that drives both.

mod agent;
mod carton;
mod client;
mod config;
mod git;
mod hub;
mod model;
mod protocol;
mod service;
mod setup;
mod store;
mod tui;
mod util;

use std::{io::IsTerminal, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use clap::{Args, Parser, Subcommand};

use crate::{
    client::Client,
    config::{Mode, QuickArgs},
    hub::{Hub, HubOptions},
    model::{Event, ProjectView, Status},
    store::Store,
    util::{Error, say},
};

#[derive(Parser)]
#[command(
    version,
    about,
    subcommand_precedence_over_arg = true,
    after_help = "\
Examples:
  tinycd                                       deploy this checkout; run the dashboard if none is
  tinycd ~/code/app                            deploy another checkout
  tinycd git@github.com:example/app.git ~/blog deploy a URL into ~/blog
  tinycd serve                                 run the dashboard and the local host
  tinycd agent https://hub:4443 --token …      join a dashboard from another machine
  tinycd init                                  password, service, GitHub App"
)]
struct Cli {
    /// Data directory. Defaults to $TINYCD_HOME, /var/lib/tinycd as root,
    /// or ~/.tinycd.
    #[arg(long, global = true, value_name = "DIR")]
    data: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    quick: QuickArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the dashboard and the local host: the default single-machine setup.
    Serve(ServeArgs),
    /// Run a host that joins a dashboard on another machine or in a container.
    Agent(AgentArgs),
    /// Set this machine up: dashboard password, service, GitHub App.
    Init,
    /// Attach the live terminal dashboard to the running hub.
    Dashboard {
        /// Focus one project's releases.
        project: Option<String>,
    },
    /// Show the deployed state.
    Status { project: Option<String> },
    /// List projects.
    Projects,
    /// Deploy the newest commit now.
    Deploy { project: Option<String> },
    /// Start an earlier release again.
    Rollback {
        project: Option<String>,
        /// The release to roll back to; the one before current by default.
        #[arg(long, value_name = "ID")]
        to: Option<String>,
    },
    /// Stop and start the current release.
    Restart { project: Option<String> },
    /// Stop the current release until the next deployment or restart.
    Stop { project: Option<String> },
    /// Build and verify a throwaway release without touching the deployed state.
    DryRun { project: Option<String> },
    /// Show a deployment's log.
    Logs {
        project: Option<String>,
        /// A release id; the newest by default.
        #[arg(long, value_name = "ID")]
        deployment: Option<String>,
        /// Keep following the log.
        #[arg(short, long)]
        follow: bool,
    },
    /// Remove a project from the dashboard.
    Remove {
        project: String,
        /// Leave whatever it runs running.
        #[arg(long)]
        keep_running: bool,
    },
    /// The machines that run projects.
    Hosts {
        #[command(subcommand)]
        action: Option<HostsCommand>,
    },
    /// The GitHub App.
    Github {
        #[command(subcommand)]
        action: GithubCommand,
    },
    /// Run tinycd as a service.
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
}

#[derive(Subcommand)]
enum HostsCommand {
    /// List hosts.
    List,
    /// Mint a join token and print the command for the new machine.
    Add { name: String },
}

#[derive(Subcommand)]
enum GithubCommand {
    /// Create the GitHub App in the browser.
    Connect {
        /// Create it under an organisation instead of your account.
        #[arg(long)]
        org: Option<String>,
    },
    /// Show whether an app is connected.
    Status,
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Write the service definition for this platform and enable it.
    Install {
        /// Install the agent role instead of the dashboard.
        #[arg(long)]
        agent: bool,
        /// Only write the definition.
        #[arg(long)]
        no_enable: bool,
    },
}

#[derive(Args)]
struct ServeArgs {
    /// Plain HTTP listener for this machine's browser and CLI.
    #[arg(long, value_name = "ADDRESS")]
    bind: Option<SocketAddr>,
    /// HTTPS listener for agents and remote browsers, such as 0.0.0.0:4443.
    #[arg(long, value_name = "ADDRESS")]
    secure_bind: Option<SocketAddr>,
    /// Do not run a host in this process; projects run on joined hosts only.
    #[arg(long)]
    no_agent: bool,
    /// Plain line output instead of the live dashboard on interactive
    /// terminals.
    #[arg(long)]
    no_tui: bool,
}

#[derive(Args)]
struct AgentArgs {
    /// The dashboard's URL, such as https://hub.example:4443. Remembered
    /// after the first run.
    url: Option<String>,
    /// The join token from the dashboard's Hosts page.
    #[arg(long, env = "TINYCD_JOIN_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// How this host appears in the dashboard. Defaults to the hostname.
    #[arg(long)]
    name: Option<String>,
    /// Allow a plain http:// dashboard on another machine.
    #[arg(long)]
    insecure: bool,
    /// Accepted for symmetry with serve; the agent prints lines anyway.
    #[arg(long, hide = true)]
    no_tui: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    util::init_crypto();
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tinycd: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    let data = match &cli.data {
        Some(dir) => dir.clone(),
        None => store::default_data_dir()?,
    };

    match cli.command {
        None => run_quick(&data, cli.quick).await,
        Some(Command::Serve(args)) => run_serve(&data, args).await,
        Some(Command::Agent(args)) => run_agent(&data, args).await,
        Some(Command::Init) => run_init(&data).await,
        Some(Command::Dashboard { project }) => {
            if !interactive() {
                return Err("the dashboard needs an interactive terminal; use `tinycd status` or the web dashboard".into());
            }
            let client = require_client(&data)?;
            let focus = match project {
                Some(project) => Some(resolve_project(&client, Some(project)).await?),
                None => None,
            };
            tui::dashboard(client, focus, "q detach").await
        }
        Some(Command::Status { project }) => run_status(&data, project).await,
        Some(Command::Projects) => {
            let client = require_client(&data)?;
            let overview = client.overview().await?;
            for view in overview.projects {
                let latest = view
                    .deployments
                    .first()
                    .map(|d| format!("{} ({} ago)", d.status.label(), util::age_of(d.updated_at)))
                    .unwrap_or_else(|| "never deployed".to_owned());
                println!(
                    "{:<24} {:<10} {:<10} {:<28} {}",
                    view.project.id,
                    view.project.host,
                    view.project.runtime.label(),
                    latest,
                    view.project.repo
                );
            }
            Ok(())
        }
        Some(Command::Deploy { project }) => {
            let client = require_client(&data)?;
            let id = resolve_project(&client, project).await?;
            let deployment = client.deploy(&id).await?;
            println!("{id}: deployment {deployment} queued");
            follow_deployment(&client, &id, &deployment).await
        }
        Some(Command::Rollback { project, to }) => {
            let client = require_client(&data)?;
            let id = resolve_project(&client, project).await?;
            let target = client.rollback(&id, to.as_deref()).await?;
            println!("{id}: rolling back to {target}");
            follow_deployment(&client, &id, &target).await
        }
        Some(Command::Restart { project }) => {
            let client = require_client(&data)?;
            let id = resolve_project(&client, project).await?;
            client.restart(&id).await?;
            println!("{id}: restarting");
            Ok(())
        }
        Some(Command::Stop { project }) => {
            let client = require_client(&data)?;
            let id = resolve_project(&client, project).await?;
            client.stop(&id).await?;
            println!("{id}: stopping");
            Ok(())
        }
        Some(Command::DryRun { project }) => {
            let client = require_client(&data)?;
            let id = resolve_project(&client, project).await?;
            println!("{id}: dry run started; this takes as long as a deployment");
            println!("{}", client.dry_run(&id).await?);
            Ok(())
        }
        Some(Command::Logs {
            project,
            deployment,
            follow,
        }) => {
            let client = require_client(&data)?;
            let id = resolve_project(&client, project).await?;
            let view = client.project(&id).await?;
            let deployment = match deployment {
                Some(deployment) => deployment,
                None => view
                    .in_progress
                    .clone()
                    .or_else(|| view.deployments.first().map(|d| d.id.clone()))
                    .ok_or("nothing has been deployed yet")?,
            };
            if follow {
                let mut lines = client.follow_log(&id, &deployment);
                while let Some(line) = lines.recv().await {
                    println!("{line}");
                }
            } else {
                print!("{}", client.log(&id, &deployment).await?);
            }
            Ok(())
        }
        Some(Command::Remove {
            project,
            keep_running,
        }) => {
            let client = require_client(&data)?;
            let id = resolve_project(&client, Some(project)).await?;
            client.remove_project(&id, !keep_running).await?;
            println!("removed {id}");
            Ok(())
        }
        Some(Command::Hosts { action }) => {
            let client = require_client(&data)?;
            match action.unwrap_or(HostsCommand::List) {
                HostsCommand::List => {
                    for host in client.overview().await?.hosts {
                        println!(
                            "{:<16} {:<20} {:<8} {:<20} {} {}",
                            host.host.id,
                            host.host.name,
                            if host.online { "online" } else { "offline" },
                            host.host
                                .runtimes
                                .iter()
                                .map(|r| r.label())
                                .collect::<Vec<_>>()
                                .join(","),
                            host.host.os,
                            host.host.version
                        );
                    }
                }
                HostsCommand::Add { name } => {
                    let info = client.join_token(&name).await?;
                    println!("On the new machine, install tinycd and run once:\n");
                    println!("  {}\n", info.command);
                    println!(
                        "The token expires in a day and pins this dashboard's certificate. \
                         Then: tinycd service install --agent"
                    );
                }
            }
            Ok(())
        }
        Some(Command::Github { action }) => {
            let client = require_client(&data)?;
            match action {
                GithubCommand::Connect { org } => {
                    let mut url = format!("{}/github/connect", client.base());
                    if let Some(org) = org {
                        url.push_str("?org=");
                        url.push_str(&org);
                    }
                    println!("Log in to the dashboard, then create the app at:\n\n  {url}\n");
                    if setup::open_browser(&url) {
                        println!("(opened in your browser)");
                    }
                }
                GithubCommand::Status => {
                    let settings = client.settings().await?;
                    match settings.get("github").filter(|g| !g.is_null()) {
                        Some(app) => println!(
                            "connected: {} ({})\ninstall on repositories: {}",
                            app["name"].as_str().unwrap_or(""),
                            app["html_url"].as_str().unwrap_or(""),
                            app["install_url"].as_str().unwrap_or("")
                        ),
                        None => println!("no GitHub App connected; run: tinycd github connect"),
                    }
                }
            }
            Ok(())
        }
        Some(Command::Service { action }) => match action {
            ServiceCommand::Install { agent, no_enable } => {
                let role = if agent {
                    service::Role::Agent
                } else {
                    service::Role::Serve
                };
                for line in service::install(role, cli.data.as_deref(), !no_enable)? {
                    println!("{line}");
                }
                Ok(())
            }
        },
    }
}

fn require_client(data: &std::path::Path) -> Result<Client, Error> {
    let store = Store::open(data)?;
    Client::attach(&store)?.ok_or_else(|| {
        format!(
            "no tinycd dashboard is running for {}; start one with `tinycd serve` (or `tinycd` inside a checkout)",
            store.dir().display()
        )
        .into()
    })
}

/// Find the project a command means: the named one, the one for the
/// current directory, or the only one.
async fn resolve_project(client: &Client, wanted: Option<String>) -> Result<String, Error> {
    let overview = client.overview().await?;
    let ids: Vec<&str> = overview
        .projects
        .iter()
        .map(|p| p.project.id.as_str())
        .collect();
    if let Some(wanted) = wanted {
        if ids.contains(&wanted.as_str()) {
            return Ok(wanted);
        }
        let by_name: Vec<&str> = overview
            .projects
            .iter()
            .filter(|p| p.project.name == wanted)
            .map(|p| p.project.id.as_str())
            .collect();
        return match by_name.as_slice() {
            [id] => Ok((*id).to_owned()),
            [] => Err(format!("no project named {wanted}; projects: {}", ids.join(", ")).into()),
            _ => Err(format!(
                "several projects are named {wanted}: {}",
                by_name.join(", ")
            )
            .into()),
        };
    }
    if let Ok(cwd) = std::env::current_dir() {
        let key = util::slug(&config::project_key(&cwd));
        if ids.contains(&key.as_str()) {
            return Ok(key);
        }
    }
    match ids.as_slice() {
        [id] => Ok((*id).to_owned()),
        [] => {
            Err("no projects yet; run tinycd inside a checkout or add one in the dashboard".into())
        }
        _ => Err(format!("name the project; projects: {}", ids.join(", ")).into()),
    }
}

/// Print the deployment's log and status changes until it settles.
async fn follow_deployment(client: &Client, project: &str, deployment: &str) -> Result<(), Error> {
    let mut events = client.events(Some(project.to_owned()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            _ = tokio::time::sleep_until(deadline) => return Err("gave up waiting after 30 minutes".into()),
        };
        match event {
            Some(Event::Log {
                deployment: which,
                line,
                ..
            }) if which == deployment => println!("  {line}"),
            Some(Event::Deployment { deployment: record }) if record.id == deployment => {
                println!("{project}: {deployment} {}", record.status.label());
                if !record.status.in_progress() {
                    return match record.status {
                        Status::Live => Ok(()),
                        _ => Err(record
                            .error
                            .unwrap_or_else(|| "the deployment did not go live".to_owned())
                            .into()),
                    };
                }
            }
            Some(_) => {}
            None => return Err("lost the connection to the dashboard".into()),
        }
    }
}

fn print_view(view: &ProjectView, daemon: bool, data: &std::path::Path) {
    let project = &view.project;
    println!("project  {} ({})", project.id, project.name);
    println!("repo     {}", project.repo);
    if let Some(git_ref) = &project.git_ref {
        println!("ref      {git_ref}");
    }
    println!("host     {} ({})", project.host, project.runtime.label());
    if let Some(root) = &project.root {
        println!("root     {}", root.display());
    }
    match &project.current {
        Some(current) => {
            let record = view.deployments.iter().find(|d| &d.id == current);
            let age = record
                .map(|d| format!(" (deployed {} ago)", util::age_of(d.updated_at)))
                .unwrap_or_default();
            println!(
                "current  {current}{age}{}",
                if project.running { "" } else { " [stopped]" }
            );
            if let Some(commit) = record.map(|d| d.commit.as_str()).filter(|c| !c.is_empty()) {
                println!("commit   {commit}");
            }
            println!(
                "log      {}",
                data.join("logs")
                    .join(&project.id)
                    .join(format!("{current}.log"))
                    .display()
            );
        }
        None => println!("current  none (never deployed)"),
    }
    if let Some(inflight) = &view.in_progress {
        println!("deploying {inflight}");
    }
    let retained = view
        .deployments
        .iter()
        .filter(|d| d.retained && !d.status.in_progress())
        .count();
    println!("releases {retained} retained");
    println!(
        "daemon   {}",
        if daemon { "running" } else { "not running" }
    );
}

async fn run_status(data: &std::path::Path, project: Option<String>) -> Result<(), Error> {
    let store = Store::open(data)?;
    match Client::attach(&store)? {
        Some(client) => {
            let id = resolve_project(&client, project).await?;
            let view = client.project(&id).await?;
            print_view(&view, true, store.dir());
        }
        None => {
            // No hub: read what it recorded.
            let projects = store.projects()?;
            let wanted = match project {
                Some(wanted) => wanted,
                None => {
                    let key = std::env::current_dir()
                        .map(|cwd| util::slug(&config::project_key(&cwd)))
                        .unwrap_or_default();
                    if projects.iter().any(|p| p.id == key) {
                        key
                    } else if projects.len() == 1 {
                        projects[0].id.clone()
                    } else {
                        return Err(format!(
                            "no dashboard is running; recorded projects: {}",
                            projects
                                .iter()
                                .map(|p| p.id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                        .into());
                    }
                }
            };
            let project = projects
                .into_iter()
                .find(|p| p.id == wanted)
                .ok_or_else(|| format!("no project named {wanted}"))?;
            let mut deployments = store.deployments(&project.id)?;
            deployments.reverse();
            print_view(
                &ProjectView {
                    project,
                    deployments,
                    in_progress: None,
                },
                false,
                store.dir(),
            );
        }
    }
    Ok(())
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

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// A hub running in this process: started in the background, reachable
/// through a client, stopped by dropping the handle's trigger.
struct Foreground {
    hub: Arc<Hub>,
    client: Client,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), Error>>,
}

impl Foreground {
    async fn start(
        store: Store,
        bind: Option<SocketAddr>,
        secure_bind: Option<SocketAddr>,
        local_agent: bool,
        echo: bool,
    ) -> Result<Self, Error> {
        let _lock = store.lock()?;
        let settings = store.settings()?;
        let bind = match bind {
            Some(bind) => bind,
            None => settings
                .bind
                .parse()
                .map_err(|error| format!("bind {:?} in settings.json: {error}", settings.bind))?,
        };
        let options = HubOptions {
            bind,
            secure_bind,
            local_agent,
            echo,
        };
        let hub = Hub::open(store, &options).await?;
        let client = Client::new(&hub.url, &hub.api_token);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let task = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move {
                let result = hub
                    .run(&options, async {
                        let _ = stop_rx.await;
                    })
                    .await;
                drop(_lock);
                result
            })
        };
        // Wait for the listener before handing out the client.
        let started = tokio::time::Instant::now();
        loop {
            if client.overview().await.is_ok() {
                break;
            }
            if task.is_finished() {
                return match task.await {
                    Ok(Err(error)) => Err(error),
                    Ok(Ok(())) => Err("the dashboard stopped before it was ready".into()),
                    Err(error) => Err(error.into()),
                };
            }
            if started.elapsed() > Duration::from_secs(10) {
                return Err("the dashboard did not come up within 10 seconds".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(Self {
            hub,
            client,
            stop: Some(stop_tx),
            task,
        })
    }

    /// Wait until the local host is attached.
    async fn wait_for_local_host(&self) {
        for _ in 0..100 {
            if self
                .hub
                .state
                .lock()
                .await
                .agents
                .contains_key(model::LOCAL_HOST)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn finish(mut self) -> Result<(), Error> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.task.await {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        }
    }

    /// Print deployment outcomes until a shutdown signal arrives; the hub
    /// echoes activity lines itself in plain mode.
    async fn print_until_signal(&self, focus: Option<String>) {
        let mut events = self.client.events(focus);
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(Event::Deployment { deployment }) if !deployment.status.in_progress() || deployment.status == Status::Syncing => {
                        match &deployment.error {
                            Some(error) => eprintln!("{}: {} {}: {error}", deployment.project, deployment.id, deployment.status.label()),
                            None => println!("{}: {} {}", deployment.project, deployment.id, deployment.status.label()),
                        }
                    }
                    Some(_) => {}
                    None => break,
                },
                () = &mut shutdown => break,
            }
        }
    }
}

async fn run_serve(data: &std::path::Path, args: ServeArgs) -> Result<(), Error> {
    let store = Store::open(data)?;
    if store.locked() {
        return Err(format!(
            "a tinycd dashboard is already running for {}; attach with `tinycd dashboard`",
            store.dir().display()
        )
        .into());
    }
    let use_tui = !args.no_tui && interactive();
    let foreground =
        Foreground::start(store, args.bind, args.secure_bind, !args.no_agent, !use_tui).await?;
    if !foreground.hub.has_password().await {
        say(format!(
            "no dashboard password yet; open {} to set one, or run tinycd init",
            foreground.hub.url
        ));
    }
    if use_tui {
        let client = foreground.client.clone();
        tokio::select! {
            result = tui::dashboard(client, None, "q quit") => result?,
            () = shutdown_signal() => ratatui::restore(),
        }
    } else {
        foreground.print_until_signal(None).await;
    }
    foreground.finish().await
}

/// The classic invocation: make this checkout (or URL) a project on the
/// local host, deploy it, and watch. A running dashboard is used if there
/// is one; otherwise one runs in this process until the user quits.
async fn run_quick(data: &std::path::Path, args: QuickArgs) -> Result<(), Error> {
    let mode = Mode::resolve(args.source.as_deref(), args.dir.as_deref())?;
    let store = Store::open(data)?;

    // First run in an unconfigured checkout on an interactive terminal:
    // guide the user through setup instead of failing on a missing start.
    if let Mode::Local { project } = &mode
        && args.config.is_none()
        && args.start.is_none()
        && !args.status
        && args.rollback.is_none()
        && !project.join(".tinycd").join("config.toml").exists()
        && interactive()
    {
        let origin = git::repo_url(None, project).await.ok();
        let container = carton::oci::OciRuntime::detect(&store.agent_dir()).await;
        let Some(answers) = setup::project_wizard(
            project,
            origin.as_deref(),
            container.as_ref().map(|r| r.cli()),
        )?
        else {
            println!("setup cancelled; run tinycd here again to retry");
            return Ok(());
        };
        let path = setup::write_config(project, &answers)?;
        println!("wrote {}", path.display());
    }

    let project = config::quick_project(&args, &mode, store.dir()).await?;
    let id = project.id.clone();
    let use_tui = !args.no_tui && interactive();

    if args.status {
        return run_status(data, Some(id)).await;
    }

    match Client::attach(&store)? {
        Some(client) => {
            let known = client.project(&id).await.ok();
            let fresh = known.is_none();
            let registered = client.upsert_project(&project).await?;
            if fresh {
                println!(
                    "added {} to the dashboard at {}",
                    registered.id,
                    client.base()
                );
            }
            if args.dry_run {
                println!("{}", client.dry_run(&id).await?);
                return Ok(());
            }
            if let Some(target) = &args.rollback {
                let target = client
                    .rollback(&id, Some(target.as_str()).filter(|t| !t.is_empty()))
                    .await?;
                println!("{id}: rolling back to {target}");
                return follow_deployment(&client, &id, &target).await;
            }
            if fresh
                || known
                    .as_ref()
                    .is_some_and(|view| view.project.current.is_none())
            {
                let deployment = client.deploy(&id).await?;
                println!("{id}: deployment {deployment} queued");
            }
            if use_tui {
                tui::dashboard(client, Some(id), "q detach (the dashboard keeps running)").await
            } else {
                let mut events = client.events(Some(id));
                let shutdown = shutdown_signal();
                tokio::pin!(shutdown);
                loop {
                    tokio::select! {
                        event = events.recv() => match event {
                            Some(Event::Message { text }) => println!("{text}"),
                            Some(Event::Deployment { deployment }) if !deployment.status.in_progress() => {
                                println!("{}: {} {}", deployment.project, deployment.id, deployment.status.label());
                            }
                            Some(_) => {}
                            None => break,
                        },
                        () = &mut shutdown => break,
                    }
                }
                Ok(())
            }
        }
        None => {
            if args.rollback.is_some() {
                return Err(
                    "rolling back needs the dashboard running: start `tinycd` or `tinycd serve` first, then `tinycd rollback`"
                        .into(),
                );
            }
            let foreground = Foreground::start(store, None, None, true, !use_tui).await?;
            foreground.wait_for_local_host().await;
            let registered = foreground.hub.upsert_project(project).await?;
            if args.dry_run {
                let result = foreground.hub.dry_run(&id).await;
                foreground.finish().await?;
                println!("{}", result?);
                return Ok(());
            }
            // A resumed release is started by the host on assignment; a
            // new project, or one with nothing deployed, deploys now.
            if registered.current.is_none() {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let current = foreground.hub.project(&id).await.and_then(|p| p.current);
                if current.is_none() {
                    foreground.hub.trigger(&id, "manual", None).await?;
                }
            }
            if use_tui {
                let client = foreground.client.clone();
                tokio::select! {
                    result = tui::dashboard(client, Some(id), "q quit (stops the dashboard; containers keep running)") => result?,
                    () = shutdown_signal() => ratatui::restore(),
                }
            } else {
                foreground.print_until_signal(Some(id)).await;
            }
            foreground.finish().await
        }
    }
}

async fn run_agent(data: &std::path::Path, args: AgentArgs) -> Result<(), Error> {
    use agent::{Agent, link};

    std::fs::create_dir_all(data)?;
    let _lock = store::lock_file(&data.join("agent.lock"))?;
    let saved = link::AgentFile::load(data);
    let file = match (&args.url, &args.token, saved) {
        (url, _, Some(mut saved))
            if saved.joined
                && url
                    .as_deref()
                    .is_none_or(|url| url.trim_end_matches('/') == saved.url) =>
        {
            // Service/container commands may keep the original one-time
            // join token in their environment. After joining, always reuse
            // the permanent credential for the same hub.
            if let Some(name) = &args.name {
                saved.name = name.clone();
            }
            saved.save(data)?;
            saved
        }
        (Some(url), Some(token), _) => {
            let (fingerprint, secret) = link::parse_join_token(token).ok_or(
                "the token does not look like a tinycd join token (tcj1.<fingerprint>.<secret>)",
            )?;
            let file = link::AgentFile {
                url: url.trim_end_matches('/').to_owned(),
                token: secret,
                fingerprint,
                name: args.name.clone().unwrap_or_else(util::hostname),
                joined: false,
            };
            file.save(data)?;
            file
        }
        (Some(url), None, Some(mut saved)) => {
            saved.url = url.trim_end_matches('/').to_owned();
            if let Some(name) = &args.name {
                saved.name = name.clone();
            }
            saved.save(data)?;
            saved
        }
        (None, _, Some(saved)) => saved,
        (Some(_), None, None) => {
            return Err("the first run needs --token from the dashboard's Hosts page".into());
        }
        (None, _, None) => {
            return Err(
                "nothing to join yet: run `tinycd agent <url> --token <join token>` once".into(),
            );
        }
    };

    let (to_hub_tx, to_hub_rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = Arc::new(Agent::new(data.join("agent"), true, to_hub_tx).await);
    tokio::spawn(Arc::clone(&agent).monitor());
    say(format!("host {} joining {}", file.name, file.url));
    let link = link::run_remote(
        Arc::clone(&agent),
        data.to_path_buf(),
        file,
        to_hub_rx,
        args.insecure,
    );
    tokio::select! {
        result = link => result,
        () = shutdown_signal() => {
            say("shutting down");
            agent.shutdown().await;
            Ok(())
        }
    }
}

async fn run_init(data: &std::path::Path) -> Result<(), Error> {
    if !interactive() {
        return Err("tinycd init needs an interactive terminal".into());
    }
    let store = Store::open(data)?;
    let running = Client::attach(&store)?;
    let has_password = match &running {
        Some(client) => {
            client.settings().await.is_ok() && store.settings()?.admin_password.is_some()
        }
        None => store.settings()?.admin_password.is_some(),
    };
    let Some(answers) = setup::node_wizard(store.dir(), has_password, running.is_some())? else {
        println!("setup cancelled");
        return Ok(());
    };

    if running.is_none() {
        let mut settings = store.settings()?;
        if let Some(password) = &answers.password {
            settings.admin_password = Some(util::hash_password(password));
        }
        if answers.public_url.is_some() {
            settings.public_url = answers.public_url.clone();
        }
        store.save_settings(&settings)?;
        println!("saved settings in {}", store.dir().display());
    } else {
        println!("a dashboard is running; set the public URL and password in its Settings page");
    }

    if answers.install_service {
        for line in service::install(service::Role::Serve, Some(store.dir()), running.is_none())? {
            println!("{line}");
        }
        if running.is_none() {
            // Give the service a moment to come up so the next steps can use it.
            for _ in 0..30 {
                if store.locked() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }

    let client = Client::attach(&store)?;
    let url = client
        .as_ref()
        .map(|c| c.base().to_owned())
        .unwrap_or_else(|| {
            format!(
                "http://{}",
                store.settings().map(|s| s.bind).unwrap_or_default()
            )
        });
    if answers.connect_github {
        let connect = format!("{url}/github/connect");
        println!("\nCreate the GitHub App at: {connect}");
        if client.is_some() && setup::open_browser(&connect) {
            println!("(opened in your browser)");
        } else if client.is_none() {
            println!("(start the dashboard first: tinycd serve)");
        }
    }
    println!("\nDashboard: {url}");
    if client.is_none() && !answers.install_service {
        println!("Start it with: tinycd serve");
    }
    Ok(())
}
