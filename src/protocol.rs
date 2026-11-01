//! Messages between the hub and its agents. The same messages travel over
//! an in-process channel when the agent shares the hub's process, and over
//! a WebSocket when it does not, so a carton behaves the same wherever it
//! runs.

use serde::{Deserialize, Serialize};

use crate::model::{Project, RuntimeKind, Status};

pub const VERSION: u32 = 1;

/// The path the hub serves the agent WebSocket on.
pub const AGENT_PATH: &str = "/agent";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub v: u32,
    #[serde(flatten)]
    pub message: T,
}

impl<T> Envelope<T> {
    pub fn new(message: T) -> Self {
        Self {
            v: VERSION,
            message,
        }
    }
}

/// A short-lived credential for one clone. Delivered to git through
/// GIT_CONFIG_* so it is never written to disk or shown in process
/// arguments.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitCredential {
    /// The full `http.extraHeader` value.
    pub header: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ToAgent {
    /// Sent once after the agent is authenticated.
    Welcome {
        host: String,
        projects: Vec<Project>,
        /// The permanent bearer token, on the connection that redeemed a
        /// join token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Add or update a project on this host.
    Assign {
        project: Project,
    },
    /// Remove a project; optionally stop what it runs.
    Unassign {
        project: String,
        stop: bool,
    },
    Deploy {
        project: String,
        deployment: String,
        /// The commit to deploy; the remote head when unset.
        commit: Option<String>,
        /// A URL to clone instead of the project's, when the hub holds a
        /// credential for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        credential: Option<GitCredential>,
    },
    /// Start a retained deployment again and make it current.
    Rollback {
        project: String,
        deployment: String,
    },
    Restart {
        project: String,
    },
    Stop {
        project: String,
    },
    /// Build and start one throwaway release, verify it, and stop it.
    DryRun {
        project: String,
        request: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        credential: Option<GitCredential>,
    },
    Ping,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHub {
    /// First message on every connection.
    Register {
        name: String,
        os: String,
        version: String,
        runtimes: Vec<RuntimeKind>,
    },
    /// A deployment moved to a new status.
    Progress {
        project: String,
        deployment: String,
        status: Status,
        commit: Option<String>,
        error: Option<String>,
    },
    Log {
        project: String,
        deployment: String,
        line: String,
    },
    /// What the host has for a project: the live deployment, whether it is
    /// running, and the deployments still on disk.
    State {
        project: String,
        current: Option<String>,
        running: bool,
        retained: Vec<String>,
    },
    DryRun {
        project: String,
        request: u64,
        ok: bool,
        message: String,
    },
    /// A line for the activity feed.
    Note {
        text: String,
    },
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_carry_the_version_and_tag() {
        let text = serde_json::to_string(&Envelope::new(ToHub::Pong)).unwrap();
        assert_eq!(text, r#"{"v":1,"type":"pong"}"#);
        let parsed: Envelope<ToAgent> =
            serde_json::from_str(r#"{"v":1,"type":"stop","project":"app"}"#).unwrap();
        assert!(matches!(parsed.message, ToAgent::Stop { project } if project == "app"));
    }
}
