//! The hub's JSON API from the command line's side. The terminal dashboard
//! and every `tinycd <verb>` go through here, so they work the same whether
//! the hub runs in this process, as a service, or in a container.

use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    model::{Event, Overview, Project, ProjectView},
    store::Store,
    util::Error,
};

#[derive(Clone)]
pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
}

#[derive(Debug, serde::Deserialize)]
pub struct JoinInfo {
    pub command: String,
}

impl Client {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http: reqwest::Client::builder()
                .user_agent(format!("tinycd/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("a plain HTTP client builds"),
        }
    }

    /// A client for the hub running against this data directory, if one is.
    pub fn attach(store: &Store) -> Result<Option<Self>, Error> {
        if !store.locked() {
            return Ok(None);
        }
        let Some(daemon) = store.daemon() else {
            return Ok(None);
        };
        Ok(Some(Self::new(&daemon.url, &store.api_token()?)))
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn check<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, Error> {
        let status = response.status();
        let text = response.text().await?;
        if status.is_success() {
            return serde_json::from_str(&text)
                .map_err(|error| format!("unexpected answer from the hub: {error}").into());
        }
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| text.trim().to_owned());
        Err(format!("{message} ({status})").into())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| format!("cannot reach the hub at {}: {error}", self.base))?;
        Self::check(response).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Option<Value>) -> Result<T, Error> {
        let mut request = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("cannot reach the hub at {}: {error}", self.base))?;
        Self::check(response).await
    }

    pub async fn overview(&self) -> Result<Overview, Error> {
        self.get("/api/overview").await
    }

    pub async fn project(&self, id: &str) -> Result<ProjectView, Error> {
        self.get(&format!("/api/projects/{id}")).await
    }

    pub async fn upsert_project(&self, project: &Project) -> Result<Project, Error> {
        self.post("/api/projects", Some(serde_json::to_value(project)?))
            .await
    }

    pub async fn remove_project(&self, id: &str, stop: bool) -> Result<(), Error> {
        let response = self
            .http
            .delete(format!("{}/api/projects/{id}?stop={stop}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::check::<Value>(response).await.map(|_| ())
    }

    pub async fn deploy(&self, id: &str) -> Result<String, Error> {
        let value: Value = self
            .post(&format!("/api/projects/{id}/deploy"), None)
            .await?;
        Ok(value["deployment"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn rollback(&self, id: &str, to: Option<&str>) -> Result<String, Error> {
        let value: Value = self
            .post(
                &format!("/api/projects/{id}/rollback"),
                Some(json!({ "to": to })),
            )
            .await?;
        Ok(value["deployment"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn restart(&self, id: &str) -> Result<(), Error> {
        self.post::<Value>(&format!("/api/projects/{id}/restart"), None)
            .await
            .map(|_| ())
    }

    pub async fn stop(&self, id: &str) -> Result<(), Error> {
        self.post::<Value>(&format!("/api/projects/{id}/stop"), None)
            .await
            .map(|_| ())
    }

    pub async fn dry_run(&self, id: &str) -> Result<String, Error> {
        let value: Value = self
            .post(&format!("/api/projects/{id}/dry-run"), None)
            .await?;
        Ok(value["message"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn log(&self, project: &str, deployment: &str) -> Result<String, Error> {
        let response = self
            .http
            .get(format!(
                "{}/api/projects/{project}/deployments/{deployment}/log",
                self.base
            ))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(format!("the hub answered {}", response.status()).into());
        }
        Ok(response.text().await?)
    }

    pub async fn join_token(&self, name: &str) -> Result<JoinInfo, Error> {
        self.post("/api/hosts", Some(json!({ "name": name }))).await
    }

    pub async fn settings(&self) -> Result<Value, Error> {
        self.get("/api/settings").await
    }

    /// Subscribe to the hub's events; the receiver ends when the hub goes
    /// away.
    pub fn events(&self, project: Option<String>) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = self.clone();
        tokio::spawn(async move {
            let mut url = format!("{}/api/events", client.base);
            if let Some(project) = project {
                url.push_str("?project=");
                url.push_str(&project);
            }
            let Ok(response) = client.http.get(url).bearer_auth(&client.token).send().await else {
                return;
            };
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = stream.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find("\n\n") {
                    let block = buffer[..end].to_owned();
                    buffer.drain(..end + 2);
                    let data: String = block
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if data.is_empty() {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<Event>(&data)
                        && tx.send(event).is_err()
                    {
                        return;
                    }
                }
            }
        });
        rx
    }

    /// Follow a deployment log line by line until it settles.
    pub fn follow_log(&self, project: &str, deployment: &str) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = self.clone();
        let url = format!(
            "{}/api/projects/{project}/deployments/{deployment}/log?follow=1",
            client.base
        );
        tokio::spawn(async move {
            let Ok(response) = client.http.get(url).bearer_auth(&client.token).send().await else {
                return;
            };
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = stream.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find("\n\n") {
                    let block = buffer[..end].to_owned();
                    buffer.drain(..end + 2);
                    let mut event = None;
                    let mut data = Vec::new();
                    for line in block.lines() {
                        if let Some(name) = line.strip_prefix("event:") {
                            event = Some(name.trim().to_owned());
                        } else if let Some(text) = line.strip_prefix("data:") {
                            data.push(text.strip_prefix(' ').unwrap_or(text).to_owned());
                        }
                    }
                    if data.is_empty() && event.is_none() {
                        continue;
                    }
                    match event.as_deref() {
                        Some("status") => {
                            let _ = tx.send(format!("== {}", data.join("\n")));
                            return;
                        }
                        _ => {
                            if tx.send(data.join("\n")).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        rx
    }
}
