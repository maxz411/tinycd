//! The deployment pipeline one host runs for one project:
//!
//! ```text
//! sync -> build -> start -> verify -> switch -> prune
//! ```
//!
//! Every deployment gets a new folder under the project's root; `current`
//! is atomically moved to the newest verified release, and older releases
//! stay for rollback.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use super::{Agent, Runner};
use crate::{
    carton::{self, CartonSpec, Runtime, log_line, redirect, shell_command},
    config::FileConfig,
    git,
    model::Status,
    protocol::GitCredential,
    util::{Error, say, say_err},
};

/// How long a started carton must stay up to count as running.
pub const STAY_UP: Duration = Duration::from_secs(3);

/// How long a configured check command gets to pass, retrying every second.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

pub fn release_log(root: &Path, id: &str) -> PathBuf {
    root.join("logs").join(format!("{id}.log"))
}

/// Open a release log for appending. Logging is best-effort: on failure the
/// commands inherit tinycd's own stdout.
pub async fn open_log(path: &Path) -> Option<std::fs::File> {
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
            say_err(format!("failed to open {}: {error}", path.display()));
            None
        }
    }
}

fn banner(log: Option<&std::fs::File>, line: &str) {
    say(line);
    log_line(log, line);
}

/// The complete deployments on disk, oldest first.
pub async fn retained(root: &Path) -> Vec<String> {
    let deployments = root.join("deployments");
    let mut releases = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(&deployments).await else {
        return releases;
    };
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
    releases
}

/// The release `current` points at, if any and if it is where it should be.
pub async fn current_release(root: &Path) -> Result<Option<String>, Error> {
    let current = root.join("current");
    match tokio::fs::canonicalize(&current).await {
        Ok(resolved) => {
            let deployments = tokio::fs::canonicalize(root.join("deployments")).await?;
            if !resolved.starts_with(deployments) {
                return Err("current points outside the deployments directory".into());
            }
            Ok(Some(
                resolved
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Resolve how this release runs: the repository's .tinycd/config.toml
/// provides the recipe, and the project's own recipe overrides it.
pub async fn run_spec(
    runner: &Runner,
    release: &Path,
    deployment: &str,
) -> Result<CartonSpec, Error> {
    let file = FileConfig::load(&release.join(".tinycd").join("config.toml"), false).await?;
    let recipe = file.recipe().overridden_by(&runner.project.recipe);
    let shell = recipe.shell.unwrap_or_else(carton::default_shell);
    if shell.is_empty() {
        return Err("shell must name a program".into());
    }
    let mut env = recipe.env;
    if let Some(port) = recipe.port
        && !env.contains_key("PORT")
    {
        env.insert("PORT".to_owned(), port.to_string());
    }
    Ok(CartonSpec {
        project: runner.project.id.clone(),
        deployment: deployment.to_owned(),
        shell,
        env,
        install: recipe.install,
        start: recipe.start.ok_or(
            "start is not configured; set it in .tinycd/config.toml (committed or in the project settings)",
        )?,
        check: recipe.check,
        port: recipe.port,
        base: recipe.base,
        dockerfile: recipe.dockerfile,
        share: runner.project.share.clone(),
        shared_dir: runner.root.join("shared"),
    })
}

/// Clone the repository into an empty release folder and report the commit
/// it is at.
async fn sync(
    runner: &Runner,
    release: &Path,
    source: &Source<'_>,
    log: Option<&std::fs::File>,
) -> Result<String, Error> {
    let project = &runner.project;
    let credential = source.credential;
    let repo = source.repo.unwrap_or(&project.repo);
    let default_sync = match (cfg!(windows), project.git_ref.is_some()) {
        (false, false) => r#"git clone --depth 1 "$TINYCD_REPO" ."#,
        (false, true) => r#"git clone --depth 1 --branch "$TINYCD_REF" "$TINYCD_REPO" ."#,
        (true, false) => r#"git clone --depth 1 "%TINYCD_REPO%" ."#,
        (true, true) => r#"git clone --depth 1 --branch "%TINYCD_REF%" "%TINYCD_REPO%" ."#,
    };
    let command = project.recipe.sync.as_deref().unwrap_or(default_sync);
    banner(log, &format!("running sync: {command}"));

    let shell = project
        .recipe
        .shell
        .clone()
        .unwrap_or_else(carton::default_shell);
    let mut process = shell_command(&shell, &project.recipe.env, command, release);
    process
        .env("TINYCD_REPO", repo)
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(git_ref) = &project.git_ref {
        process.env("TINYCD_REF", git_ref);
    }
    git::apply_credential(&mut process, credential);
    redirect(&mut process, log)?;
    let status = process.status().await?;
    if !status.success() {
        return Err(format!("sync command exited with {status}").into());
    }

    match git::checked_out_commit(release).await {
        Some(commit) => Ok(commit),
        None => git::remote_head(repo, project.git_ref.as_deref(), credential).await,
    }
}

/// Where a deployment clones from: the project's URL, or one the hub
/// substituted along with a credential for it.
#[derive(Clone, Copy, Default)]
pub struct Source<'a> {
    pub repo: Option<&'a str>,
    pub credential: Option<&'a GitCredential>,
}

/// A started carton must stay up for STAY_UP, and pass the check command
/// within CHECK_TIMEOUT when one is configured. Returns the carton still
/// running; on failure nothing is left running.
async fn verify_start(
    agent: &Agent,
    runner: &Runner,
    deployment: &str,
    mut carton: carton::Carton,
    spec: &CartonSpec,
    release: &Path,
    log: Option<&std::fs::File>,
) -> Result<carton::Carton, Error> {
    let result = async {
        if let Ok(exit) = tokio::time::timeout(STAY_UP, carton.wait()).await {
            return Err(
                format!("{} within {} seconds of starting", exit?, STAY_UP.as_secs()).into(),
            );
        }

        if let Some(check) = &spec.check {
            agent.progress(&runner.project.id, deployment, Status::Checking, None, None);
            banner(log, &format!("running check: {check}"));
            let deadline = tokio::time::Instant::now() + CHECK_TIMEOUT;
            let mut check_spec = spec.clone();
            check_spec.start = check.clone();
            loop {
                if let Some(exit) = carton.exited().await? {
                    return Err(format!("{exit} during the check").into());
                }
                let mut checker =
                    carton::process::ProcessRuntime.start(release, &check_spec, log)?;
                let outcome = tokio::time::timeout_at(deadline, checker.wait_status()).await;
                checker.stop().await?;
                if let Ok(status) = outcome
                    && status?.success()
                {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!(
                        "the check command did not pass within {} seconds",
                        CHECK_TIMEOUT.as_secs()
                    )
                    .into());
                }
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + Duration::from_secs(1)).min(deadline),
                )
                .await;
            }
        }
        Ok::<(), Error>(())
    }
    .await;
    match result {
        Ok(()) => Ok(carton),
        Err(error) => {
            let _ = carton.stop().await;
            Err(error)
        }
    }
}

/// Sync, build, start, verify, and switch one deployment.
pub async fn deploy(
    agent: &Agent,
    runner: &mut Runner,
    id: &str,
    source: Source<'_>,
) -> Result<(), Error> {
    let project_id = runner.project.id.clone();
    let root = runner.root.clone();
    let runtime = agent.runtime_for(&runner.project)?;

    agent.progress(&project_id, id, Status::Syncing, None, None);
    wait_for_interlock(&runner.interlock()).await?;

    let deployments = root.join("deployments");
    let release = deployments.join(id);
    // Releases are built under their final name: renaming a folder after
    // install would break the absolute paths that Python and Node tooling
    // bake into virtualenvs and shebangs. The marker distinguishes a
    // completed release from one interrupted mid-build.
    let marker = deployments.join(format!(".incomplete-{id}"));
    tokio::fs::create_dir_all(&deployments).await?;
    tokio::fs::write(&marker, "").await?;
    tokio::fs::create_dir(&release).await?;
    let log_path = release_log(&root, id);
    let log = open_log(&log_path).await;
    agent.tail_log(runner, id, true);
    let cite = |error: Error| format!("{error} (log: {})", log_path.display());

    let built = async {
        let commit = sync(runner, &release, &source, log.as_ref()).await?;
        agent.progress(
            &project_id,
            id,
            Status::Building,
            Some(commit.clone()),
            None,
        );
        let spec = run_spec(runner, &release, id).await?;
        runtime.build(&release, &spec, log.as_ref()).await?;
        Ok::<_, Error>((commit, spec))
    };
    let (commit, spec) = match built.await {
        Ok(built) => built,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&release).await;
            let _ = tokio::fs::remove_file(&marker).await;
            return Err(cite(error).into());
        }
    };

    // The interlock may have appeared during a slow build; recheck before
    // touching the running carton.
    wait_for_interlock(&runner.interlock()).await?;
    tokio::fs::remove_file(&marker).await?;

    agent.progress(&project_id, id, Status::Starting, None, None);
    if let Some(previous) = runner.running.take() {
        previous.stop().await?;
    }
    banner(log.as_ref(), &format!("running start: {}", spec.start));
    let carton = match runtime.start(&release, &spec, log.as_ref()).await {
        Ok(carton) => carton,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&release).await;
            restart_previous(agent, runner, &runtime).await;
            return Err(cite(error).into());
        }
    };
    match verify_start(agent, runner, id, carton, &spec, &release, log.as_ref()).await {
        Ok(carton) => runner.running = Some(carton),
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&release).await;
            restart_previous(agent, runner, &runtime).await;
            return Err(cite(error).into());
        }
    }

    if let Err(error) = point_current(&root, id) {
        if let Some(carton) = runner.running.take() {
            let _ = carton.stop().await;
        }
        return Err(format!("failed to update the current deployment: {error}").into());
    }
    if let Err(error) = tokio::fs::write(root.join("head"), format!("{commit}\n")).await {
        say_err(format!("failed to record the deployed commit: {error}"));
    }
    runner.current = Some(id.to_owned());
    runner.head = Some(commit.clone());
    runner.restarts = 0;

    prune(&deployments, &release, runner.project.keep).await;
    let keep = retained(&root).await;
    runtime.prune(&project_id, &keep).await;
    prune_logs(&root.join("logs"), runner.project.keep).await;
    say(format!("deployed {}", release.display()));
    agent.progress(&project_id, id, Status::Live, Some(commit), None);
    Ok(())
}

/// Bring the release that current points at back up after a failed deploy.
async fn restart_previous(agent: &Agent, runner: &mut Runner, runtime: &Runtime) {
    let root = runner.root.clone();
    let Ok(Some(id)) = current_release(&root).await else {
        return;
    };
    let current = root.join("current");
    let log = open_log(&release_log(&root, &id)).await;
    match run_spec(runner, &current, &id).await {
        Ok(spec) => {
            banner(log.as_ref(), "restarting the previous release");
            match runtime.start(&current, &spec, log.as_ref()).await {
                Ok(carton) => {
                    runner.running = Some(carton);
                    agent.tail_log(runner, &id, false);
                }
                Err(error) => say_err(format!("failed to restart the previous release: {error}")),
            }
        }
        Err(error) => say_err(format!("failed to restart the previous release: {error}")),
    }
}

/// Start the release that current points at, if a previous run left one.
/// Containers that are still running are adopted rather than restarted.
pub async fn resume(agent: &Agent, runner: &mut Runner) -> Result<(), Error> {
    let root = runner.root.clone();
    let Some(id) = current_release(&root).await? else {
        return Ok(());
    };
    runner.current = Some(id.clone());
    runner.head = tokio::fs::read_to_string(root.join("head"))
        .await
        .ok()
        .map(|head| head.trim().to_owned())
        .filter(|head| !head.is_empty());
    if runner.running.is_some() {
        return Ok(());
    }

    let runtime = agent.runtime_for(&runner.project)?;
    wait_for_interlock(&runner.interlock()).await?;
    let current = root.join("current");
    let spec = run_spec(runner, &current, &id).await?;
    let log = open_log(&release_log(&root, &id)).await;
    let carton = match runtime.adopt(&spec, log.as_ref()).await? {
        Some(carton) => {
            banner(log.as_ref(), "adopted the running container");
            carton
        }
        None => {
            banner(log.as_ref(), &format!("running start: {}", spec.start));
            runtime.start(&current, &spec, log.as_ref()).await?
        }
    };
    runner.running = Some(carton);
    agent.tail_log(runner, &id, false);
    Ok(())
}

/// Start a retained release again and make it current. The recorded head
/// is left alone so the rollback survives until the remote actually
/// changes.
pub async fn rollback(agent: &Agent, runner: &mut Runner, target: &str) -> Result<(), Error> {
    let root = runner.root.clone();
    let project_id = runner.project.id.clone();
    let releases = retained(&root).await;
    if !releases.iter().any(|id| id == target) {
        return Err(format!(
            "release {target} is not retained; retained: {}",
            releases.join(", ")
        )
        .into());
    }
    let runtime = agent.runtime_for(&runner.project)?;
    let release = root.join("deployments").join(target);
    let spec = run_spec(runner, &release, target).await?;
    if !runtime.built(&release, &spec).await {
        return Err(format!("release {target} has no build to start").into());
    }

    wait_for_interlock(&runner.interlock()).await?;
    let log = open_log(&release_log(&root, target)).await;
    agent.tail_log(runner, target, false);
    agent.progress(&project_id, target, Status::Starting, None, None);
    if let Some(previous) = runner.running.take() {
        previous.stop().await?;
    }
    banner(
        log.as_ref(),
        &format!("rolling back: running start: {}", spec.start),
    );
    let carton = match runtime.start(&release, &spec, log.as_ref()).await {
        Ok(carton) => carton,
        Err(error) => {
            restart_previous(agent, runner, &runtime).await;
            return Err(error);
        }
    };
    match verify_start(agent, runner, target, carton, &spec, &release, log.as_ref()).await {
        Ok(carton) => runner.running = Some(carton),
        Err(error) => {
            restart_previous(agent, runner, &runtime).await;
            return Err(error);
        }
    }
    point_current(&root, target)?;
    runner.current = Some(target.to_owned());
    runner.restarts = 0;
    say(format!("rolled back to {}", release.display()));
    agent.progress(&project_id, target, Status::Live, None, None);
    Ok(())
}

/// Stop whatever runs, then start current again.
pub async fn restart(agent: &Agent, runner: &mut Runner) -> Result<(), Error> {
    if let Some(carton) = runner.running.take() {
        carton.stop().await?;
    }
    resume(agent, runner).await
}

pub async fn stop(runner: &mut Runner) -> Result<(), Error> {
    if let Some(carton) = runner.running.take() {
        carton.stop().await?;
        say(format!("stopped {}", runner.project.id));
    }
    Ok(())
}

/// Build and start one throwaway release under <root>/.dry-run-<id>,
/// verify it the same way a deployment would, then stop it. The deployed
/// state is never touched, so a dry run is safe next to a live release
/// (though the started server may contend for the same port).
pub async fn dry_run(
    agent: &Agent,
    runner: &mut Runner,
    source: Source<'_>,
) -> Result<String, Error> {
    let root = runner.root.clone();
    let runtime = agent.runtime_for(&runner.project)?;
    let id = format!("dry-{}", crate::util::now_ms());
    let scratch = root.join(format!(".dry-run-{id}"));
    let release = scratch.join("release");
    tokio::fs::create_dir_all(&release).await?;

    let result = async {
        sync(runner, &release, &source, None).await?;
        let spec = run_spec(runner, &release, &id).await?;
        runtime.build(&release, &spec, None).await?;
        banner(None, &format!("running start: {}", spec.start));
        let carton = runtime.start(&release, &spec, None).await?;
        let carton = verify_start(agent, runner, &id, carton, &spec, &release, None).await?;
        carton.stop().await?;
        Ok::<(), Error>(())
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&scratch).await;
    let keep = retained(&root).await;
    runtime.prune(&runner.project.id, &keep).await;
    result?;
    Ok(format!(
        "dry run passed: the release built, started, stayed up for {} seconds, and stopped cleanly",
        STAY_UP.as_secs()
    ))
}

/// Atomically repoint the current symlink at the release with this id.
#[cfg(not(windows))]
pub fn point_current(root: &Path, id: &str) -> std::io::Result<()> {
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
pub fn point_current(root: &Path, id: &str) -> std::io::Result<()> {
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
                        say_err(format!("failed to remove {}: {error}", path.display()));
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
            say_err(format!("failed to remove {}: {error}", old.display()));
        }
    }
}

async fn remove(path: &Path) {
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        say_err(format!("failed to remove {}: {error}", path.display()));
    }
}

pub async fn wait_for_interlock(path: &Path) -> Result<(), Error> {
    let mut waiting = false;

    loop {
        match tokio::fs::symlink_metadata(path).await {
            Ok(_) => {
                if !waiting {
                    say(format!(
                        "waiting for interlock to disappear: {}",
                        path.display()
                    ));
                    waiting = true;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if waiting {
                    say(format!("interlock removed: {}", path.display()));
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
