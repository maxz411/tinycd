//! The GitHub App: created by the user through GitHub's manifest flow, so
//! the dashboard owns its own app with no third party in between. It
//! provides push webhooks, short-lived clone credentials for private
//! repositories, and commit statuses.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use super::{Hub, tls};
use crate::{
    git,
    model::{Deployment, GitHubApp, Project, Status},
    protocol::GitCredential,
    util::{self, Error, say_err},
};

const API: &str = "https://api.github.com";

pub fn webhook_path() -> &'static str {
    "/api/github/webhook"
}

pub fn install_url(app: &GitHubApp) -> String {
    format!("https://github.com/apps/{}/installations/new", app.slug)
}

/// The manifest GitHub turns into an app. The webhook is only active when
/// the dashboard has a public URL for GitHub to reach.
pub fn manifest(name: &str, hub_url: &str, public_url: Option<&str>) -> serde_json::Value {
    let hook_url = public_url
        .map(|url| format!("{}{}", url.trim_end_matches('/'), webhook_path()))
        .unwrap_or_else(|| format!("{}{}", hub_url.trim_end_matches('/'), webhook_path()));
    json!({
        "name": name,
        "url": public_url.unwrap_or(hub_url),
        "description": "Deployments by tinycd",
        "hook_attributes": { "url": hook_url, "active": public_url.is_some() },
        "redirect_url": format!("{}/github/callback", hub_url.trim_end_matches('/')),
        "public": false,
        "default_permissions": {
            "contents": "read",
            "metadata": "read",
            "statuses": "write"
        },
        "default_events": ["push"]
    })
}

#[derive(Deserialize)]
struct Conversion {
    id: u64,
    slug: String,
    name: String,
    client_id: String,
    client_secret: String,
    webhook_secret: String,
    pem: String,
    html_url: String,
    owner: Owner,
}

#[derive(Deserialize)]
struct Owner {
    login: String,
}

/// Exchange the code GitHub redirects back with for the app's credentials.
pub async fn convert(hub: &Hub, code: &str) -> Result<GitHubApp, Error> {
    let response = hub
        .http
        .post(format!("{API}/app-manifests/{code}/conversions"))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("GitHub did not answer: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub rejected the manifest code: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        )
        .into());
    }
    let conversion: Conversion = response.json().await?;
    Ok(GitHubApp {
        id: conversion.id,
        slug: conversion.slug,
        name: conversion.name,
        owner: conversion.owner.login,
        html_url: conversion.html_url,
        client_id: conversion.client_id,
        client_secret: conversion.client_secret,
        webhook_secret: conversion.webhook_secret,
        pem: conversion.pem,
    })
}

/// A JWT that authenticates as the app itself, good for ten minutes.
pub fn app_jwt(app: &GitHubApp) -> Result<String, Error> {
    let der = tls::pem_to_der(&app.pem).ok_or("the app's private key is not valid PEM")?;
    let key = if app.pem.contains("BEGIN RSA PRIVATE KEY") {
        ring::rsa::KeyPair::from_der(&der)
    } else {
        ring::rsa::KeyPair::from_pkcs8(&der)
    }
    .map_err(|error| format!("the app's private key was rejected: {error}"))?;

    let now = util::now_ms() / 1000;
    let header = util::base64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = util::base64url(
        json!({ "iat": now - 60, "exp": now + 540, "iss": app.id })
            .to_string()
            .as_bytes(),
    );
    let signing_input = format!("{header}.{payload}");
    let rng = ring::rand::SystemRandom::new();
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &rng,
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "failed to sign the app token")?;
    Ok(format!("{signing_input}.{}", util::base64url(&signature)))
}

fn request(
    hub: &Hub,
    method: reqwest::Method,
    path: &str,
    bearer: &str,
) -> reqwest::RequestBuilder {
    hub.http
        .request(method, format!("{API}{path}"))
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .bearer_auth(bearer)
}

/// The installation that covers a repository, if the app is installed
/// there. Cached briefly, since polling asks often.
async fn installation_id(
    hub: &Hub,
    app: &GitHubApp,
    owner: &str,
    repo: &str,
) -> Result<Option<u64>, Error> {
    let key = format!("{}/{}", owner.to_lowercase(), repo.to_lowercase());
    if let Some(cached) = hub.cached_installation(&key).await {
        return Ok(cached);
    }
    let jwt = app_jwt(app)?;
    let response = request(
        hub,
        reqwest::Method::GET,
        &format!("/repos/{owner}/{repo}/installation"),
        &jwt,
    )
    .send()
    .await?;
    let id = match response.status().as_u16() {
        200 => {
            #[derive(Deserialize)]
            struct Installation {
                id: u64,
            }
            Some(response.json::<Installation>().await?.id)
        }
        404 => None,
        status => {
            return Err(format!(
                "GitHub answered {status} when looking up the installation for {owner}/{repo}"
            )
            .into());
        }
    };
    hub.cache_installation(&key, id).await;
    Ok(id)
}

/// A token for one installation, cached until shortly before it expires.
async fn installation_token(
    hub: &Hub,
    app: &GitHubApp,
    installation: u64,
) -> Result<String, Error> {
    if let Some(token) = hub.cached_installation_token(installation).await {
        return Ok(token);
    }
    let jwt = app_jwt(app)?;
    let response = request(
        hub,
        reqwest::Method::POST,
        &format!("/app/installations/{installation}/access_tokens"),
        &jwt,
    )
    .send()
    .await?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub refused an installation token: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        )
        .into());
    }
    #[derive(Deserialize)]
    struct Token {
        token: String,
    }
    let token = response.json::<Token>().await?.token;
    // Installation tokens last an hour.
    hub.cache_installation_token(installation, token.clone(), util::now_ms() + 3600 * 1000)
        .await;
    Ok(token)
}

/// If the app covers the project's repository, the HTTPS URL to clone and
/// the header that authenticates it. The project's own URL may be SSH; the
/// app makes it HTTPS so the token applies.
pub async fn credential_for(hub: &Hub, project: &Project) -> Option<(String, GitCredential)> {
    let app = hub.state.lock().await.settings.github.clone()?;
    let (owner, repo) = git::github_repo(&project.repo)?;
    let installation = match installation_id(hub, &app, &owner, &repo).await {
        Ok(Some(id)) => id,
        Ok(None) => return None,
        Err(error) => {
            say_err(format!("{}: {error}", project.id));
            return None;
        }
    };
    let token = match installation_token(hub, &app, installation).await {
        Ok(token) => token,
        Err(error) => {
            say_err(format!("{}: {error}", project.id));
            return None;
        }
    };
    let basic = util::base64_standard(format!("x-access-token:{token}").as_bytes());
    Some((
        format!("https://github.com/{owner}/{repo}.git"),
        GitCredential {
            header: format!("AUTHORIZATION: basic {basic}"),
        },
    ))
}

/// Mirror a deployment's outcome as a commit status, when the app covers
/// the repository. Runs in the background; failures only log.
pub fn report_status(hub: Arc<Hub>, project: Project, deployment: Deployment) {
    let state = match deployment.status {
        Status::Syncing => "pending",
        Status::Live => "success",
        Status::Failed => "failure",
        _ => return,
    };
    if deployment.commit.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let Some(app) = hub.state.lock().await.settings.github.clone() else {
            return;
        };
        let Some((owner, repo)) = git::github_repo(&project.repo) else {
            return;
        };
        let result = async {
            let Some(installation) = installation_id(&hub, &app, &owner, &repo).await? else {
                return Ok(());
            };
            let token = installation_token(&hub, &app, installation).await?;
            let mut description = match (&deployment.error, deployment.status) {
                (Some(error), _) => format!("tinycd: {error}"),
                (None, Status::Live) => format!("tinycd: live on {}", project.host),
                (None, _) => "tinycd: deploying".to_owned(),
            };
            if description.len() > 140 {
                description.truncate(137);
                description.push_str("...");
            }
            let target = format!(
                "{}/projects/{}",
                hub.public_url().await.trim_end_matches('/'),
                project.id
            );
            let response = request(
                &hub,
                reqwest::Method::POST,
                &format!("/repos/{owner}/{repo}/statuses/{}", deployment.commit),
                &token,
            )
            .json(&json!({
                "state": state,
                "target_url": target,
                "description": description,
                "context": format!("tinycd/{}", project.id),
            }))
            .send()
            .await?;
            if !response.status().is_success() {
                return Err(format!(
                    "GitHub refused the status: {} {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                )
                .into());
            }
            Ok::<(), Error>(())
        }
        .await;
        if let Err(error) = result {
            say_err(format!(
                "{}: could not report to GitHub: {error}",
                project.id
            ));
        }
    });
}

/// Point the app's webhook at the dashboard's public URL.
pub async fn update_webhook(hub: &Hub, app: &GitHubApp, public_url: &str) -> Result<(), Error> {
    let jwt = app_jwt(app)?;
    let url = format!("{}{}", public_url.trim_end_matches('/'), webhook_path());
    let response = request(hub, reqwest::Method::PATCH, "/app/hook/config", &jwt)
        .json(&json!({ "url": url }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub refused the webhook update: {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        )
        .into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct Push {
    #[serde(rename = "ref")]
    git_ref: String,
    after: String,
    #[serde(default)]
    deleted: bool,
    repository: PushRepository,
}

#[derive(Deserialize)]
struct PushRepository {
    full_name: String,
    default_branch: String,
}

/// Act on a verified webhook delivery. Returns what was done, for the
/// response body GitHub shows in its delivery log.
pub async fn handle_webhook(hub: &Hub, event: &str, body: &[u8]) -> Result<String, Error> {
    match event {
        "push" => {}
        "ping" => return Ok("pong".to_owned()),
        other => return Ok(format!("ignored {other}")),
    }
    let push: Push = serde_json::from_slice(body)
        .map_err(|error| format!("unreadable push payload: {error}"))?;
    if push.deleted || push.after.chars().all(|c| c == '0') {
        return Ok("ignored a deletion".to_owned());
    }
    let pushed = push
        .git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| push.git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(&push.git_ref);

    let matching: Vec<String> = {
        let state = hub.state.lock().await;
        state
            .projects
            .values()
            .filter(|project| {
                git::github_repo(&project.repo).is_some_and(|(owner, repo)| {
                    format!("{owner}/{repo}").eq_ignore_ascii_case(&push.repository.full_name)
                })
            })
            .filter(|project| match &project.git_ref {
                Some(wanted) => wanted == pushed,
                None => pushed == push.repository.default_branch,
            })
            .map(|project| project.id.clone())
            .collect()
    };
    if matching.is_empty() {
        return Ok(format!(
            "no project deploys {} of {}",
            pushed, push.repository.full_name
        ));
    }
    let mut done = Vec::new();
    for id in matching {
        match hub.trigger(&id, "webhook", Some(push.after.clone())).await {
            Ok(deployment) => done.push(format!("{id}: deployment {deployment}")),
            Err(error) => done.push(format!("{id}: {error}")),
        }
    }
    Ok(done.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Throwaway keys generated for this test; never used anywhere.
    const PKCS1: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAzfmQkQnzeMGLMj19jutU2JM+7GO0r7mUzNfRKCBOoSe+nETY\nCs4PB2uqT877uCM3Xlsn5Z0crwCQBAgBSAiKzYFjlFyBWAs+SxpnanyWXaIOFY9m\nMGa/oe8p6yf8hySUKLxUJpXT5UAm6MGLlezQBAbD6GCA933d6piZ94EfzYNwEQ0G\nAT99SZWSajZ/b85/u8Ojpg63YLUBjjslg7CPIGVY3zUDgth4Dq0WLjnywiTKLKDs\n5E/0IPTxukqW4sGtjreWC0sIGEPQ5piKN6h5s5Hcnw04Fu9KVuvVlRe8JqDGA3F7\nndFb0+Bu7wxMo4SlMAcagCrkfPRfGtlkM2Q20QIDAQABAoIBAAvnVeMD87WxXcWI\nBzcYw50GmXbY5krXi1FxUY2jnFe078+hLOkm8nhn0P5uxF5mbp893mbnNw6oFiqd\nedF7aOYqiaI73F/FupJlkr91Ns1vfafiUzIN5hpKM556/52h0Awwoh5ev1G67OS7\ntsDx4C2zZGQMG7fx5r7VleRXaoqaaO3D1jnuh1TNenxru8eygsIP35arNyk8Ioys\n+jpWNF/qttibKGAwlNEiXo5XftCLC4RUDBE+DwjLituh266Xrim8NMlPRawNhSnn\nAM1/Cva8i5P8u2nkUeMXMi8eKXpkc+SSbOIEKxuu4L+FPV85GOEXumUTLODcVpb5\nHiwpqhcCgYEA8jKrRC+34f5JrFtvVV5nXoU9Z9nKeGIWfvMn7bs3x0p5Tq44R6ia\nFfF+azo3QOFKkUNs82nLSDFJUCcVbp6xtzwyqJ6qbmTYzxZVEhscJwlqqLZNeiCZ\noor6PqubaC5doAmK6qKMKH2mZtoHdOv1XSqH6Qb6to7pDVWI82ercEsCgYEA2bZz\nmgXt2olkNjooCsczzC3L14YwLuCoDheTTxSmsbmpv9HjBElMVx2tN/eOObMJDwNj\n3qAfvWsWAjfPchyzNd0FhHRuHIA0ewJNoLMmmfziNIPqme2Q2JptiEqOb5OxIg3i\n0W7qJboxwaAnwrLl5FiXqXXWdLHoWDRits0EW9MCgYEAqL6OvxI0HrvJ3tFRz3vX\nr25Lqzh9rcRBgi+t6R+grDSCMkSGJ78bpCA+gTztVEHjbzQ4C5vxKxpTxwmEEC93\nCIBjl0H84q9Ws2oO2W+T/Mq9ylBByM4/3JTR56ouu7ZaOu+LHxEjb+6jalOMEHV5\n3XCNgtj/AwSs8UJgCZ54PuMCgYBMPif3zZHBkZHlp7I0LBTIsVUMkjK3h/dyj/EH\npbzGyrVWtsMg0iaI75yk1+4X6MoIVvCXm03iQOlBN5hPZxufhlqTS07wbjlyxpi2\n6WZgUP5K09PJujeak2I+c5W0H23N9jzziTs4oR8hptulxI7by+a48sp6rX8nvJjB\nWcTn/wKBgQDXw8PqxH7bSNtjWwlett3BlGnD/6yNpcpo0KBNyvJuM0Ph64vbzokf\nZP9temQvZP6OmzMlciDgqKIzmNKd3QmRnIdchlDPQSAYcHaCAeH2lt3+z32P3qiw\n1nY8G1j/SZitzaUHX23gGoEMsbHlkMYZ6UBHxsq1BBNIk/YX6ScbmA==\n-----END RSA PRIVATE KEY-----";
    const PKCS8: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDN+ZCRCfN4wYsy\nPX2O61TYkz7sY7SvuZTM19EoIE6hJ76cRNgKzg8Ha6pPzvu4IzdeWyflnRyvAJAE\nCAFICIrNgWOUXIFYCz5LGmdqfJZdog4Vj2YwZr+h7ynrJ/yHJJQovFQmldPlQCbo\nwYuV7NAEBsPoYID3fd3qmJn3gR/Ng3ARDQYBP31JlZJqNn9vzn+7w6OmDrdgtQGO\nOyWDsI8gZVjfNQOC2HgOrRYuOfLCJMosoOzkT/Qg9PG6Spbiwa2Ot5YLSwgYQ9Dm\nmIo3qHmzkdyfDTgW70pW69WVF7wmoMYDcXud0VvT4G7vDEyjhKUwBxqAKuR89F8a\n2WQzZDbRAgMBAAECggEAC+dV4wPztbFdxYgHNxjDnQaZdtjmSteLUXFRjaOcV7Tv\nz6Es6SbyeGfQ/m7EXmZunz3eZuc3DqgWKp150Xto5iqJojvcX8W6kmWSv3U2zW99\np+JTMg3mGkoznnr/naHQDDCiHl6/Ubrs5Lu2wPHgLbNkZAwbt/HmvtWV5Fdqippo\n7cPWOe6HVM16fGu7x7KCwg/flqs3KTwijKz6OlY0X+q22JsoYDCU0SJejld+0IsL\nhFQMET4PCMuK26HbrpeuKbw0yU9FrA2FKecAzX8K9ryLk/y7aeRR4xcyLx4pemRz\n5JJs4gQrG67gv4U9XzkY4Re6ZRMs4NxWlvkeLCmqFwKBgQDyMqtEL7fh/kmsW29V\nXmdehT1n2cp4YhZ+8yftuzfHSnlOrjhHqJoV8X5rOjdA4UqRQ2zzactIMUlQJxVu\nnrG3PDKonqpuZNjPFlUSGxwnCWqotk16IJmiivo+q5toLl2gCYrqoowofaZm2gd0\n6/VdKofpBvq2jukNVYjzZ6twSwKBgQDZtnOaBe3aiWQ2OigKxzPMLcvXhjAu4KgO\nF5NPFKaxuam/0eMESUxXHa039445swkPA2PeoB+9axYCN89yHLM13QWEdG4cgDR7\nAk2gsyaZ/OI0g+qZ7ZDYmm2ISo5vk7EiDeLRbuolujHBoCfCsuXkWJepddZ0sehY\nNGK2zQRb0wKBgQCovo6/EjQeu8ne0VHPe9evbkurOH2txEGCL63pH6CsNIIyRIYn\nvxukID6BPO1UQeNvNDgLm/ErGlPHCYQQL3cIgGOXQfzir1azag7Zb5P8yr3KUEHI\nzj/clNHnqi67tlo674sfESNv7qNqU4wQdXndcI2C2P8DBKzxQmAJnng+4wKBgEw+\nJ/fNkcGRkeWnsjQsFMixVQySMreH93KP8QelvMbKtVa2wyDSJojvnKTX7hfoyghW\n8JebTeJA6UE3mE9nG5+GWpNLTvBuOXLGmLbpZmBQ/krT08m6N5qTYj5zlbQfbc32\nPPOJOzihHyGm26XEjtvL5rjyynqtfye8mMFZxOf/AoGBANfDw+rEfttI22NbCV62\n3cGUacP/rI2lymjQoE3K8m4zQ+Hri9vOiR9k/216ZC9k/o6bMyVyIOCoojOY0p3d\nCZGch1yGUM9BIBhwdoIB4faW3f7PfY/eqLDWdjwbWP9JmK3NpQdfbeAagQyxseWQ\nxhnpQEfGyrUEE0iT9hfpJxuY\n-----END PRIVATE KEY-----";

    fn app(pem: &str) -> GitHubApp {
        GitHubApp {
            id: 12345,
            pem: pem.to_owned(),
            ..GitHubApp::default()
        }
    }

    #[test]
    fn app_jwts_are_signed_from_either_pem_format() {
        for pem in [PKCS1, PKCS8] {
            let jwt = app_jwt(&app(pem)).unwrap();
            let parts: Vec<&str> = jwt.split('.').collect();
            assert_eq!(parts.len(), 3);
            let decode = |text: &str| {
                use base64::Engine;
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(text)
                    .unwrap()
            };
            assert_eq!(decode(parts[0]), br#"{"alg":"RS256","typ":"JWT"}"#);
            let payload: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
            assert_eq!(payload["iss"], 12345);
            assert!(payload["exp"].as_u64().unwrap() > payload["iat"].as_u64().unwrap());
            assert_eq!(decode(parts[2]).len(), 256);
        }
        assert!(app_jwt(&app("not a key")).is_err());
    }

    #[test]
    fn manifest_activates_the_hook_only_with_a_public_url() {
        let private = manifest("x", "http://127.0.0.1:4000", None);
        assert_eq!(private["hook_attributes"]["active"], false);
        let public = manifest(
            "x",
            "http://127.0.0.1:4000",
            Some("https://deploy.example.com/"),
        );
        assert_eq!(public["hook_attributes"]["active"], true);
        assert_eq!(
            public["hook_attributes"]["url"],
            "https://deploy.example.com/api/github/webhook"
        );
        assert_eq!(
            public["redirect_url"],
            "http://127.0.0.1:4000/github/callback"
        );
    }
}
