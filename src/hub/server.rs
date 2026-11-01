//! The hub's HTTP surface: the web dashboard, the JSON API the CLI and the
//! terminal dashboard use, webhooks, and the WebSocket agents connect to.
//! The same router serves plain HTTP on loopback and HTTPS elsewhere.

use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    body::Bytes,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response, sse},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::{Hub, github, web};
use crate::{
    model::{Event, Project},
    protocol::{AGENT_PATH, Envelope, ToAgent, ToHub},
    util::{self, Error, html_escape},
};

const SESSION_COOKIE: &str = "tinycd_session";

pub fn router(hub: Arc<Hub>) -> Router {
    let protected = Router::new()
        .route("/", get(page_overview))
        .route("/logout", post(logout))
        .route("/projects/new", get(page_project_new))
        .route("/projects", post(form_project_create))
        .route(
            "/projects/{id}",
            get(page_project).post(form_project_update),
        )
        .route("/projects/{id}/deploy", post(form_deploy))
        .route("/projects/{id}/rollback", post(form_rollback))
        .route("/projects/{id}/restart", post(form_restart))
        .route("/projects/{id}/stop", post(form_stop))
        .route("/projects/{id}/delete", post(form_delete))
        .route(
            "/projects/{id}/deployments/{deployment}",
            get(page_deployment),
        )
        .route("/hosts", get(page_hosts).post(form_host_add))
        .route("/settings", get(page_settings).post(form_settings))
        .route("/github/connect", get(page_github_connect))
        .route("/github/disconnect", post(form_github_disconnect))
        .route("/api/overview", get(api_overview))
        .route("/api/events", get(api_events))
        .route("/api/projects", get(api_projects).post(api_project_upsert))
        .route(
            "/api/projects/{id}",
            get(api_project).delete(api_project_delete),
        )
        .route("/api/projects/{id}/deploy", post(api_deploy))
        .route("/api/projects/{id}/rollback", post(api_rollback))
        .route("/api/projects/{id}/restart", post(api_restart))
        .route("/api/projects/{id}/stop", post(api_stop))
        .route("/api/projects/{id}/dry-run", post(api_dry_run))
        .route(
            "/api/projects/{id}/deployments/{deployment}/log",
            get(api_log),
        )
        .route("/api/hosts", get(api_hosts).post(api_host_join))
        .route("/api/settings", get(api_settings))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&hub),
            require_auth,
        ));

    Router::new()
        .route("/login", get(page_login).post(form_login))
        .route("/setup", get(page_setup).post(form_setup))
        .route("/static/style.css", get(style))
        // The one-time state authenticates GitHub's cross-site callback;
        // a SameSite=Strict session cookie cannot accompany it.
        .route("/github/callback", get(github_callback))
        .route("/api/github/webhook", post(github_webhook))
        .route("/api/hooks/{id}", post(project_hook))
        .route(AGENT_PATH, get(agent_socket))
        .merge(protected)
        .with_state(hub)
}

// ----- authentication ---------------------------------------------------

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.trim().to_owned())
    })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

async fn require_auth(
    State(hub): State<Arc<Hub>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let api = request.uri().path().starts_with("/api/");
    if bearer(headers).is_some_and(|token| hub.check_api_token(token))
        || session_cookie(headers).is_some_and(|id| hub.session_valid(&id))
    {
        return next.run(request).await;
    }
    if api {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response();
    }
    if hub.has_password().await {
        Redirect::to("/login").into_response()
    } else {
        Redirect::to("/setup").into_response()
    }
}

fn set_session(hub: &Hub) -> [(header::HeaderName, String); 1] {
    let id = hub.new_session();
    [(
        header::SET_COOKIE,
        format!("{SESSION_COOKIE}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000"),
    )]
}

#[derive(Deserialize)]
struct Flash {
    #[serde(default)]
    ok: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn redirect_with(path: &str, ok: Result<String, Error>) -> Redirect {
    let (key, text) = match ok {
        Ok(text) => ("ok", text),
        Err(error) => ("error", error.to_string()),
    };
    let query = url_encode(&text);
    let joiner = if path.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{path}{joiner}{key}={query}"))
}

fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ----- pages --------------------------------------------------------------

async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        web::STYLE,
    )
}

async fn page_login(State(hub): State<Arc<Hub>>, Query(flash): Query<Flash>) -> Response {
    if !hub.has_password().await {
        return Redirect::to("/setup").into_response();
    }
    Html(web::login(flash.error.as_deref())).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn form_login(
    State(hub): State<Arc<Hub>>,
    axum::Form(form): axum::Form<LoginForm>,
) -> Response {
    if hub.check_password(&form.password).await {
        (set_session(&hub), Redirect::to("/")).into_response()
    } else {
        redirect_with("/login", Err("wrong password".into())).into_response()
    }
}

async fn logout(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(id) = session_cookie(&headers) {
        hub.end_session(&id);
    }
    (
        [(
            header::SET_COOKIE,
            format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
        )],
        Redirect::to("/login"),
    )
        .into_response()
}

async fn page_setup(State(hub): State<Arc<Hub>>, Query(flash): Query<Flash>) -> Response {
    if hub.has_password().await {
        return Redirect::to("/login").into_response();
    }
    Html(web::setup(flash.error.as_deref())).into_response()
}

#[derive(Deserialize)]
struct SetupForm {
    password: String,
    confirm: String,
}

async fn form_setup(
    State(hub): State<Arc<Hub>>,
    axum::Form(form): axum::Form<SetupForm>,
) -> Response {
    if hub.has_password().await {
        return Redirect::to("/login").into_response();
    }
    if form.password != form.confirm {
        return redirect_with("/setup", Err("the passwords do not match".into())).into_response();
    }
    match hub.set_password(&form.password).await {
        Ok(()) => (set_session(&hub), Redirect::to("/")).into_response(),
        Err(error) => redirect_with("/setup", Err(error)).into_response(),
    }
}

async fn page_overview(State(hub): State<Arc<Hub>>, Query(flash): Query<Flash>) -> Html<String> {
    let overview = hub.overview().await;
    Html(web::overview(&overview, &flash_pair(&flash)))
}

fn flash_pair(flash: &Flash) -> web::Flash {
    web::Flash {
        ok: flash.ok.clone(),
        error: flash.error.clone(),
    }
}

async fn page_project_new(State(hub): State<Arc<Hub>>, Query(flash): Query<Flash>) -> Html<String> {
    let overview = hub.overview().await;
    let settings = hub.settings().await;
    let project = Project {
        poll: Some(settings.poll),
        ..Project::default()
    };
    Html(web::project_form(
        &overview,
        &project,
        true,
        &flash_pair(&flash),
    ))
}

async fn page_project(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Query(flash): Query<Flash>,
) -> Response {
    let Some(view) = hub.project_view(&id).await else {
        return (StatusCode::NOT_FOUND, "no such project").into_response();
    };
    let overview = hub.overview().await;
    let focus = view
        .in_progress
        .clone()
        .or_else(|| view.project.current.clone());
    let log = match &focus {
        Some(deployment) => hub.read_log(&id, deployment).await,
        None => String::new(),
    };
    let settings = hub.settings().await;
    Html(web::project(
        &overview,
        &view,
        focus.as_deref(),
        &log,
        &settings,
        &hub.url,
        &flash_pair(&flash),
    ))
    .into_response()
}

async fn page_deployment(
    State(hub): State<Arc<Hub>>,
    Path((id, deployment)): Path<(String, String)>,
) -> Response {
    let Some(view) = hub.project_view(&id).await else {
        return (StatusCode::NOT_FOUND, "no such project").into_response();
    };
    let Some(record) = view
        .deployments
        .iter()
        .find(|d| d.id == deployment)
        .cloned()
    else {
        return (StatusCode::NOT_FOUND, "no such deployment").into_response();
    };
    let log = hub.read_log(&id, &deployment).await;
    Html(web::deployment(&view, &record, &log)).into_response()
}

async fn page_hosts(State(hub): State<Arc<Hub>>, Query(flash): Query<Flash>) -> Html<String> {
    let overview = hub.overview().await;
    Html(web::hosts(&overview, None, &flash_pair(&flash)))
}

#[derive(Deserialize)]
struct HostForm {
    name: String,
}

async fn form_host_add(
    State(hub): State<Arc<Hub>>,
    axum::Form(form): axum::Form<HostForm>,
) -> Response {
    match hub.create_join_token(&form.name).await {
        Ok((token, url)) => {
            let overview = hub.overview().await;
            let flash = web::Flash::default();
            Html(web::hosts(
                &overview,
                Some((&form.name, &token, &url)),
                &flash,
            ))
            .into_response()
        }
        Err(error) => redirect_with("/hosts", Err(error)).into_response(),
    }
}

async fn page_settings(State(hub): State<Arc<Hub>>, Query(flash): Query<Flash>) -> Html<String> {
    let overview = hub.overview().await;
    let settings = hub.settings().await;
    Html(web::settings(
        &overview,
        &settings,
        &hub.tls.fingerprint,
        &flash_pair(&flash),
    ))
}

#[derive(Deserialize)]
struct SettingsForm {
    #[serde(default)]
    public_url: String,
    #[serde(default)]
    poll: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

async fn form_settings(
    State(hub): State<Arc<Hub>>,
    axum::Form(form): axum::Form<SettingsForm>,
) -> Response {
    let result = async {
        let public_url = form.public_url.trim().trim_end_matches('/').to_owned();
        if !public_url.is_empty()
            && !public_url.starts_with("http://")
            && !public_url.starts_with("https://")
        {
            return Err::<String, Error>(
                "the public URL must start with http:// or https://".into(),
            );
        }
        let poll: u64 = if form.poll.trim().is_empty() {
            30
        } else {
            form.poll
                .trim()
                .parse()
                .map_err(|_| "the poll interval must be a number of seconds")?
        };
        if poll == 0 {
            return Err("the poll interval must be at least one second".into());
        }
        if !form.password.is_empty() {
            if form.password != form.confirm {
                return Err("the passwords do not match".into());
            }
            hub.set_password(&form.password).await?;
        }
        let previous = hub.settings().await;
        let settings = hub
            .update_settings(|settings| {
                settings.public_url = (!public_url.is_empty()).then_some(public_url.clone());
                settings.poll = poll;
            })
            .await?;
        if let (Some(app), Some(url)) = (&settings.github, &settings.public_url)
            && previous.public_url.as_deref() != Some(url.as_str())
        {
            github::update_webhook(&hub, app, url).await?;
            return Ok("saved; the GitHub App's webhook now points at the public URL. If the app was created without a public URL, enable Active in its GitHub webhook settings".to_owned());
        }
        Ok("saved".to_owned())
    }
    .await;
    redirect_with("/settings", result).into_response()
}

#[derive(Deserialize)]
struct ConnectQuery {
    #[serde(default)]
    org: String,
    #[serde(default)]
    name: String,
}

async fn page_github_connect(
    State(hub): State<Arc<Hub>>,
    Query(query): Query<ConnectQuery>,
) -> Html<String> {
    let settings = hub.settings().await;
    let state = hub.github_state();
    let name = if query.name.trim().is_empty() {
        format!("tinycd-{}", util::hostname().to_lowercase())
    } else {
        query.name.trim().to_owned()
    };
    let manifest = github::manifest(&name, &hub.url, settings.public_url.as_deref());
    let org = query.org.trim();
    let action = if org.is_empty() {
        format!("https://github.com/settings/apps/new?state={state}")
    } else {
        format!(
            "https://github.com/organizations/{}/settings/apps/new?state={state}",
            url_encode(org)
        )
    };
    Html(web::github_connect(&action, &manifest.to_string()))
}

#[derive(Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
}

async fn github_callback(
    State(hub): State<Arc<Hub>>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if !hub.take_github_state(&query.state) {
        return redirect_with(
            "/settings",
            Err("the GitHub callback did not match a pending connection; try again".into()),
        )
        .into_response();
    }
    let result = async {
        let app = github::convert(&hub, &query.code).await?;
        let install = github::install_url(&app);
        hub.forget_github().await;
        hub.update_settings(|settings| settings.github = Some(app))
            .await?;
        Ok::<_, Error>(format!(
            "GitHub App created. Install it on your repositories at {install}"
        ))
    }
    .await;
    redirect_with("/settings", result).into_response()
}

async fn form_github_disconnect(State(hub): State<Arc<Hub>>) -> Response {
    hub.forget_github().await;
    let result = hub
        .update_settings(|settings| settings.github = None)
        .await
        .map(|_| "GitHub App forgotten; delete it from GitHub's developer settings too".to_owned());
    redirect_with("/settings", result).into_response()
}

// ----- project forms -----------------------------------------------------

async fn form_project_create(
    State(hub): State<Arc<Hub>>,
    axum::Form(form): axum::Form<web::ProjectForm>,
) -> Response {
    match form.into_project(None) {
        Ok(project) => match hub.upsert_project(project).await {
            Ok(project) => {
                let id = project.id.clone();
                let result = hub
                    .trigger(&id, "manual", None)
                    .await
                    .map(|_| "created; deploying".to_owned());
                redirect_with(&format!("/projects/{id}"), result).into_response()
            }
            Err(error) => redirect_with("/projects/new", Err(error)).into_response(),
        },
        Err(error) => redirect_with("/projects/new", Err(error)).into_response(),
    }
}

async fn form_project_update(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    axum::Form(form): axum::Form<web::ProjectForm>,
) -> Response {
    let existing = hub.project(&id).await;
    let result = match form.into_project(existing.as_ref()) {
        Ok(mut project) => {
            project.id = id.clone();
            hub.upsert_project(project)
                .await
                .map(|_| "settings saved".to_owned())
        }
        Err(error) => Err(error),
    };
    redirect_with(&format!("/projects/{id}"), result).into_response()
}

async fn form_deploy(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    let result = hub
        .trigger(&id, "manual", None)
        .await
        .map(|deployment| format!("deployment {deployment} queued"));
    redirect_with(&format!("/projects/{id}"), result).into_response()
}

#[derive(Deserialize)]
struct RollbackForm {
    #[serde(default)]
    to: String,
}

async fn form_rollback(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    axum::Form(form): axum::Form<RollbackForm>,
) -> Response {
    let result = hub
        .rollback(&id, Some(form.to.as_str()).filter(|t| !t.is_empty()))
        .await
        .map(|target| format!("rolling back to {target}"));
    redirect_with(&format!("/projects/{id}"), result).into_response()
}

async fn form_restart(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    let result = hub.restart(&id).await.map(|()| "restarting".to_owned());
    redirect_with(&format!("/projects/{id}"), result).into_response()
}

async fn form_stop(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    let result = hub.stop(&id).await.map(|()| "stopping".to_owned());
    redirect_with(&format!("/projects/{id}"), result).into_response()
}

async fn form_delete(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.remove_project(&id, true).await {
        Ok(()) => redirect_with("/", Ok(format!("removed {id}"))).into_response(),
        Err(error) => redirect_with(&format!("/projects/{id}"), Err(error)).into_response(),
    }
}

// ----- JSON API -------------------------------------------------------------

fn api_error(error: Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({ "error": error.to_string() })),
    )
        .into_response()
}

async fn api_overview(State(hub): State<Arc<Hub>>) -> impl IntoResponse {
    axum::Json(hub.overview().await)
}

async fn api_projects(State(hub): State<Arc<Hub>>) -> impl IntoResponse {
    axum::Json(hub.overview().await.projects)
}

async fn api_project(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.project_view(&id).await {
        Some(view) => axum::Json(view).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no such project" })),
        )
            .into_response(),
    }
}

async fn api_project_upsert(
    State(hub): State<Arc<Hub>>,
    axum::Json(project): axum::Json<Project>,
) -> Response {
    match hub.upsert_project(project).await {
        Ok(project) => axum::Json(project).into_response(),
        Err(error) => api_error(error),
    }
}

#[derive(Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    stop: Option<bool>,
}

async fn api_project_delete(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> Response {
    match hub.remove_project(&id, query.stop.unwrap_or(true)).await {
        Ok(()) => axum::Json(json!({ "removed": id })).into_response(),
        Err(error) => api_error(error),
    }
}

async fn api_deploy(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.trigger(&id, "manual", None).await {
        Ok(deployment) => axum::Json(json!({ "deployment": deployment })).into_response(),
        Err(error) => api_error(error),
    }
}

#[derive(Deserialize, Default)]
struct RollbackBody {
    #[serde(default)]
    to: Option<String>,
}

async fn api_rollback(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    body: Option<axum::Json<RollbackBody>>,
) -> Response {
    let to = body.and_then(|body| body.0.to);
    match hub.rollback(&id, to.as_deref()).await {
        Ok(target) => axum::Json(json!({ "deployment": target })).into_response(),
        Err(error) => api_error(error),
    }
}

async fn api_restart(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.restart(&id).await {
        Ok(()) => axum::Json(json!({ "restarting": id })).into_response(),
        Err(error) => api_error(error),
    }
}

async fn api_stop(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.stop(&id).await {
        Ok(()) => axum::Json(json!({ "stopping": id })).into_response(),
        Err(error) => api_error(error),
    }
}

async fn api_dry_run(State(hub): State<Arc<Hub>>, Path(id): Path<String>) -> Response {
    match hub.dry_run(&id).await {
        Ok(message) => axum::Json(json!({ "ok": true, "message": message })).into_response(),
        Err(error) => api_error(error),
    }
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    follow: Option<String>,
}

async fn api_log(
    State(hub): State<Arc<Hub>>,
    Path((id, deployment)): Path<(String, String)>,
    Query(query): Query<LogQuery>,
) -> Response {
    let existing = hub.read_log(&id, &deployment).await;
    if query.follow.is_none() {
        return (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            existing,
        )
            .into_response();
    }
    let rx = hub.subscribe();
    let backlog: Vec<String> = existing.lines().map(str::to_owned).collect();
    let stream =
        futures_util::stream::unfold((backlog.into_iter(), rx), move |(mut backlog, mut rx)| {
            let id = id.clone();
            let deployment = deployment.clone();
            async move {
                if let Some(line) = backlog.next() {
                    return Some((
                        Ok::<_, std::convert::Infallible>(sse::Event::default().data(line)),
                        (backlog, rx),
                    ));
                }
                loop {
                    match rx.recv().await {
                        Ok(Event::Log {
                            project,
                            deployment: which,
                            line,
                        }) if project == id && which == deployment => {
                            return Some((Ok(sse::Event::default().data(line)), (backlog, rx)));
                        }
                        Ok(Event::Deployment { deployment: record })
                            if record.project == id
                                && record.id == deployment
                                && !record.status.in_progress() =>
                        {
                            return Some((
                                Ok(sse::Event::default()
                                    .event("status")
                                    .data(record.status.label())),
                                (backlog, rx),
                            ));
                        }
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return None,
                    }
                }
            }
        });
    sse::Sse::new(stream)
        .keep_alive(sse::KeepAlive::default())
        .into_response()
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    project: Option<String>,
}

async fn api_events(State(hub): State<Arc<Hub>>, Query(query): Query<EventsQuery>) -> Response {
    let rx = hub.subscribe();
    let filter = query.project;
    let stream = futures_util::stream::unfold(rx, move |mut rx| {
        let filter = filter.clone();
        async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let relevant = match (&filter, &event) {
                            (None, _) => true,
                            (Some(id), Event::Log { project, .. }) => project == id,
                            (Some(id), Event::Deployment { deployment }) => {
                                &deployment.project == id
                            }
                            (Some(id), Event::Project { project }) => &project.id == id,
                            (Some(id), Event::ProjectRemoved { project }) => project == id,
                            (Some(_), Event::Message { .. } | Event::Host { .. }) => true,
                        };
                        if !relevant {
                            continue;
                        }
                        let data = match sse::Event::default().json_data(&event) {
                            Ok(data) => data,
                            Err(_) => continue,
                        };
                        return Some((Ok::<_, std::convert::Infallible>(data), rx));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                }
            }
        }
    });
    sse::Sse::new(stream)
        .keep_alive(sse::KeepAlive::default())
        .into_response()
}

async fn api_hosts(State(hub): State<Arc<Hub>>) -> impl IntoResponse {
    axum::Json(hub.overview().await.hosts)
}

#[derive(Deserialize)]
struct JoinBody {
    name: String,
}

async fn api_host_join(
    State(hub): State<Arc<Hub>>,
    axum::Json(body): axum::Json<JoinBody>,
) -> Response {
    match hub.create_join_token(&body.name).await {
        Ok((token, url)) => axum::Json(json!({
            "token": token,
            "url": url,
            "command": format!("tinycd agent {url} --token {token}"),
        }))
        .into_response(),
        Err(error) => api_error(error),
    }
}

async fn api_settings(State(hub): State<Arc<Hub>>) -> impl IntoResponse {
    let settings = hub.settings().await;
    axum::Json(json!({
        "public_url": settings.public_url,
        "secure_bind": settings.secure_bind,
        "poll": settings.poll,
        "fingerprint": hub.tls.fingerprint,
        "github": settings.github.as_ref().map(|app| json!({
            "id": app.id,
            "slug": app.slug,
            "name": app.name,
            "owner": app.owner,
            "html_url": app.html_url,
            "install_url": github::install_url(app),
        })),
    }))
}

// ----- webhooks ---------------------------------------------------------

/// Accept the token however the sender can deliver it: a bearer header, a
/// GitLab webhook's X-Gitlab-Token, or a GitHub webhook's HMAC signature of
/// the body. Comparisons run over digests in constant time.
pub fn webhook_authorized(headers: &HeaderMap, body: &[u8], token: Option<&str>) -> bool {
    use subtle::ConstantTimeEq;

    let Some(token) = token else {
        return false;
    };
    if let Some(presented) = bearer(headers)
        && util::secret_eq(presented, token)
    {
        return true;
    }
    if let Some(presented) = headers
        .get("x-gitlab-token")
        .and_then(|value| value.to_str().ok())
        && util::secret_eq(presented, token)
    {
        return true;
    }
    if let Some(value) = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        && let Some(hex) = value.strip_prefix("sha256=")
        && let Some(signature) = util::decode_hex32(hex)
    {
        let computed = util::hmac_sha256(token.as_bytes(), body);
        if bool::from(signature.ct_eq(&computed)) {
            return true;
        }
    }
    false
}

async fn github_webhook(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    let secret = hub.settings().await.github.map(|app| app.webhook_secret);
    let signature_ok = headers.get("x-hub-signature-256").is_some()
        && webhook_authorized(&headers, &body, secret.as_deref());
    if !signature_ok {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let event = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    match github::handle_webhook(&hub, &event, &body).await {
        Ok(text) => (StatusCode::OK, text).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

/// A project's own webhook, for forges without an app: any POST that
/// carries the project's token deploys, so it doubles as a redeploy button.
async fn project_hook(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let token = hub
        .project(&id)
        .await
        .and_then(|project| project.webhook_token);
    if !webhook_authorized(&headers, &body, token.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response();
    }
    match hub.trigger(&id, "webhook", None).await {
        Ok(deployment) => (StatusCode::OK, format!("deploying {deployment}")).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

// ----- agents -----------------------------------------------------------

async fn agent_socket(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "a host token is required").into_response();
    };
    let Some((host, issued)) = hub.authenticate_agent(token).await else {
        return (StatusCode::UNAUTHORIZED, "unknown host token").into_response();
    };
    upgrade
        .on_upgrade(move |socket| agent_session(hub, socket, host, issued))
        .into_response()
}

async fn agent_session(hub: Arc<Hub>, mut socket: WebSocket, host: String, issued: Option<String>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<ToAgent>();
    let connection = hub.attach_agent(&host, tx, issued).await;
    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(message) = outgoing else { break };
                let Ok(text) = serde_json::to_string(&Envelope::new(message)) else { continue };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Envelope<ToHub>>(text.as_str()) {
                            Ok(envelope) => hub.handle_agent(&host, envelope.message).await,
                            Err(error) => util::say_err(format!("host {host} sent an unreadable message: {error}")),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    hub.detach_agent(&host, Some(connection)).await;
}

#[allow(dead_code)]
fn escape(text: &str) -> String {
    html_escape(text)
}

#[allow(dead_code)]
fn query_map(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_tokens_are_accepted_in_every_form() {
        let token = "0123456789abcdef0123456789abcdef";
        let body = b"{}";
        let mut headers = HeaderMap::new();
        assert!(!webhook_authorized(&headers, body, Some(token)));

        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(webhook_authorized(&headers, body, Some(token)));
        assert!(!webhook_authorized(&headers, body, Some("other")));
        assert!(!webhook_authorized(&headers, body, None));

        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", token.parse().unwrap());
        assert!(webhook_authorized(&headers, body, Some(token)));

        let mut headers = HeaderMap::new();
        let signature = util::hex(&util::hmac_sha256(token.as_bytes(), body));
        headers.insert(
            "x-hub-signature-256",
            format!("sha256={signature}").parse().unwrap(),
        );
        assert!(webhook_authorized(&headers, body, Some(token)));
        assert!(!webhook_authorized(&headers, b"{ }", Some(token)));
    }

    #[test]
    fn url_encoding() {
        assert_eq!(url_encode("a b&c=d/é"), "a+b%26c%3Dd%2F%C3%A9");
    }
}
