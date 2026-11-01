//! Polling: the trigger that works everywhere, including behind NAT. Each
//! project with a poll interval has its remote head compared against the
//! last commit it deployed or attempted.

use std::{collections::HashMap, sync::Arc, time::Duration};

use super::Hub;
use crate::{git, util::say_err};

/// Run forever, polling each project on its own interval. Polls for
/// different projects overlap; a project is never polled twice at once.
pub async fn run(hub: Arc<Hub>) {
    let mut due: HashMap<String, tokio::time::Instant> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = tokio::time::Instant::now();
        let candidates: Vec<(String, u64)> = {
            let state = hub.state.lock().await;
            state
                .projects
                .values()
                .filter_map(|project| project.poll.map(|seconds| (project.id.clone(), seconds)))
                .filter(|(id, _)| {
                    !state.inflight.contains_key(id) && !state.pending.contains_key(id)
                })
                .filter(|(id, _)| {
                    state
                        .projects
                        .get(id)
                        .is_some_and(|project| state.agents.contains_key(&project.host))
                })
                .filter(|(id, _)| !state.polling.contains(id))
                .collect()
        };
        due.retain(|id, _| candidates.iter().any(|(candidate, _)| candidate == id));

        for (id, seconds) in candidates {
            let next = due.entry(id.clone()).or_insert(now);
            if *next > now {
                continue;
            }
            *next = now + Duration::from_secs(seconds.max(1));
            let hub = Arc::clone(&hub);
            hub.state.lock().await.polling.insert(id.clone());
            tokio::spawn(async move {
                poll_once(&hub, &id).await;
                hub.state.lock().await.polling.remove(&id);
            });
        }
    }
}

async fn poll_once(hub: &Hub, id: &str) {
    let Some(project) = hub.state.lock().await.projects.get(id).cloned() else {
        return;
    };
    let (repo, credential) = hub.clone_source(&project).await;
    match git::remote_head(&repo, project.git_ref.as_deref(), credential.as_ref()).await {
        Ok(head) => {
            let outdated = {
                let state = hub.state.lock().await;
                match state.projects.get(id) {
                    Some(project) => project.head.as_deref() != Some(head.as_str()),
                    None => false,
                }
            };
            if outdated && let Err(error) = hub.trigger(id, "poll", Some(head)).await {
                say_err(format!("failed to deploy {id}: {error}"));
            }
        }
        Err(error) => say_err(format!("failed to poll {}: {error}", project.repo)),
    }
}
