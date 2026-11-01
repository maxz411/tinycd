//! Git invocations that never see tinycd's own secrets, and that receive a
//! short-lived credential through GIT_CONFIG_* rather than the command line.

use tokio::process::Command;

use crate::{protocol::GitCredential, util::Error};

/// A git command. With a credential, git adds the Authorization header on
/// every HTTPS request without persisting it anywhere.
pub fn git(args: &[&str], credential: Option<&GitCredential>) -> Command {
    let mut command = Command::new("git");
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("TINYCD_TOKEN")
        .env_remove("TINYCD_JOIN_TOKEN")
        .env_remove("TINYCD_REPO")
        .env_remove("TINYCD_REF");
    apply_credential(&mut command, credential);
    command
}

/// Hand a credential to any command that runs git underneath, such as a
/// sync command through the shell.
pub fn apply_credential(command: &mut Command, credential: Option<&GitCredential>) {
    if let Some(credential) = credential {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", &credential.header);
    }
}

/// The commit the configured ref (or HEAD) points at on the remote.
/// Branches sort before tags in ls-remote output, so a name naming both
/// picks the branch.
pub async fn remote_head(
    repo: &str,
    git_ref: Option<&str>,
    credential: Option<&GitCredential>,
) -> Result<String, Error> {
    let target = git_ref.unwrap_or("HEAD");
    let output = git(&["ls-remote", repo, target], credential)
        .output()
        .await
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git ls-remote exited with {}: {}",
            output.status,
            stderr.trim()
        )
        .into());
    }

    match String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
    {
        Some(head) => Ok(head.to_owned()),
        None => Err(format!("{repo} has no ref {target}").into()),
    }
}

/// The commit a checkout is at, if it is one.
pub async fn checked_out_commit(dir: &std::path::Path) -> Option<String> {
    let output = git(&["rev-parse", "HEAD"], None)
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!commit.is_empty()).then_some(commit)
}

/// Expand a remote name (or the default, origin) into its URL via the Git
/// project being tracked. Values that are not a known remote are used
/// as-is.
pub async fn repo_url(configured: Option<String>, dir: &std::path::Path) -> Result<String, Error> {
    let remote = configured.as_deref().unwrap_or("origin");
    let output = git(&["remote", "get-url", remote], None)
        .current_dir(dir)
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        _ => Ok(configured.ok_or_else(|| {
            format!(
                "{} is not a Git checkout with an origin remote; pass a Git URL or set repo in .tinycd/config.toml",
                dir.display()
            )
        })?),
    }
}

/// The GitHub owner and repository named by a clone URL, if it is one.
pub fn github_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_owned(), repo.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls() {
        let expected = Some(("maxz411".to_owned(), "tinycd".to_owned()));
        assert_eq!(github_repo("git@github.com:maxz411/tinycd.git"), expected);
        assert_eq!(github_repo("https://github.com/maxz411/tinycd"), expected);
        assert_eq!(
            github_repo("https://github.com/maxz411/tinycd.git/"),
            expected
        );
        assert_eq!(
            github_repo("ssh://git@github.com/maxz411/tinycd.git"),
            expected
        );
        assert_eq!(github_repo("https://gitlab.com/maxz411/tinycd.git"), None);
        assert_eq!(github_repo("https://github.com/maxz411"), None);
    }
}
