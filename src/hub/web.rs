//! The web dashboard, rendered on the server from the same records the
//! API serves. A few lines of script keep pages live through the event
//! stream; nothing needs a build step.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::github;
use crate::{
    model::{Deployment, Overview, Project, ProjectView, Recipe, RuntimeKind, Settings, Status},
    util::{Error, age_of, html_escape as h},
};

pub const STYLE: &str = r#"
:root { --bg:#0b0c0f; --panel:#15171c; --line:#262931; --fg:#e7e9ee; --muted:#8b919e; --accent:#7c9cff; --ok:#3ecf8e; --warn:#f5b64c; --bad:#ff6b6b; --mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace; }
@media (prefers-color-scheme: light) { :root { --bg:#f6f7f9; --panel:#ffffff; --line:#e1e4ea; --fg:#15171c; --muted:#5f6675; --accent:#3b5bdb; } }
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--fg); font:15px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif; }
a { color:var(--accent); text-decoration:none; } a:hover { text-decoration:underline; }
header { display:flex; align-items:center; gap:24px; padding:14px 28px; border-bottom:1px solid var(--line); background:var(--panel); }
header .brand { font-weight:700; letter-spacing:.02em; color:var(--fg); }
header nav a { color:var(--muted); margin-right:18px; } header nav a.active, header nav a:hover { color:var(--fg); }
header form { margin-left:auto; }
main { max-width:1100px; margin:0 auto; padding:24px 28px 64px; }
h1 { font-size:22px; margin:0 0 16px; } h2 { font-size:16px; margin:28px 0 10px; color:var(--muted); font-weight:600; }
.flash { padding:10px 14px; border-radius:8px; margin-bottom:18px; border:1px solid var(--line); background:var(--panel); }
.flash.error { border-color:var(--bad); } .flash.ok { border-color:var(--ok); }
.card { background:var(--panel); border:1px solid var(--line); border-radius:10px; padding:16px 18px; margin-bottom:14px; }
.grid { display:grid; grid-template-columns:repeat(auto-fill,minmax(min(100%,320px),1fr)); gap:14px; }
.row { display:flex; align-items:center; gap:12px; flex-wrap:wrap; }
.muted { color:var(--muted); } .mono { font-family:var(--mono); font-size:13px; }
.pill { display:inline-block; padding:1px 9px; border-radius:999px; font-size:12px; border:1px solid var(--line); color:var(--muted); }
.pill.live { color:var(--ok); border-color:var(--ok); } .pill.failed, .pill.cancelled { color:var(--bad); border-color:var(--bad); }
.pill.queued, .pill.syncing, .pill.building, .pill.starting, .pill.checking { color:var(--warn); border-color:var(--warn); }
.pill.online { color:var(--ok); border-color:var(--ok); } .pill.offline { color:var(--muted); }
table { width:100%; border-collapse:collapse; } th, td { text-align:left; padding:8px 10px; border-bottom:1px solid var(--line); vertical-align:top; } th { color:var(--muted); font-weight:500; font-size:13px; }
button, .button { background:var(--accent); color:#fff; border:0; border-radius:7px; padding:7px 13px; font:inherit; cursor:pointer; }
button.quiet, .button.quiet { background:transparent; color:var(--fg); border:1px solid var(--line); } button.danger { background:var(--bad); }
form.inline { display:inline; }
label { display:block; margin:12px 0 4px; font-size:13px; color:var(--muted); }
input, select, textarea { width:100%; padding:8px 10px; border-radius:7px; border:1px solid var(--line); background:var(--bg); color:var(--fg); font:inherit; }
textarea, input.mono { font-family:var(--mono); font-size:13px; }
.two { display:grid; grid-template-columns:1fr 1fr; gap:0 18px; }
pre.log { background:#000; color:#d6dae3; padding:14px; border-radius:10px; font-family:var(--mono); font-size:12.5px; line-height:1.45; max-height:480px; overflow:auto; white-space:pre-wrap; word-break:break-all; margin:0; }
.activity { max-height:260px; overflow:auto; font-family:var(--mono); font-size:12.5px; }
.activity div { padding:3px 0; border-bottom:1px solid var(--line); color:var(--muted); }
code { font-family:var(--mono); font-size:13px; background:var(--bg); padding:1px 5px; border-radius:5px; }
.token { word-break:break-all; }
details summary { cursor:pointer; color:var(--muted); }
@media (max-width:600px) {
  header { gap:12px; padding:14px 16px; flex-wrap:wrap; }
  header nav a { margin-right:12px; }
  main { padding:20px 16px 48px; }
  .two { grid-template-columns:1fr; }
  table { display:block; overflow-x:auto; }
}
"#;

const SCRIPT: &str = r#"
(function(){
  var project = document.body.dataset.project || '';
  var logFor = document.body.dataset.deployment || '';
  var pane = document.getElementById('log');
  var feed = document.getElementById('activity');
  var timer = null;
  function reload(){ if (timer) return; timer = setTimeout(function(){ location.reload(); }, 1200); }
  var es = new EventSource('/api/events' + (project ? '?project=' + encodeURIComponent(project) : ''));
  es.onmessage = function(e){
    var ev = JSON.parse(e.data);
    if (ev.type === 'log') {
      if (pane && ev.deployment === logFor) { pane.textContent += ev.line + '\n'; pane.scrollTop = pane.scrollHeight; }
    } else if (ev.type === 'message') {
      if (feed) { var d = document.createElement('div'); d.textContent = ev.text; feed.appendChild(d); feed.scrollTop = feed.scrollHeight; }
    } else { reload(); }
  };
})();
"#;

#[derive(Default, Clone)]
pub struct Flash {
    pub ok: Option<String>,
    pub error: Option<String>,
}

fn layout(
    title: &str,
    active: &str,
    body: &str,
    project: Option<&str>,
    deployment: Option<&str>,
    flash: &Flash,
) -> String {
    let nav = [
        ("/", "Projects"),
        ("/hosts", "Hosts"),
        ("/settings", "Settings"),
    ]
    .iter()
    .map(|(href, label)| {
        let class = if *href == active {
            " class=\"active\""
        } else {
            ""
        };
        format!("<a href=\"{href}\"{class}>{label}</a>")
    })
    .collect::<String>();
    let flashes = flash
        .error
        .as_ref()
        .map(|text| format!("<div class=\"flash error\">{}</div>", h(text)))
        .unwrap_or_default()
        + &flash
            .ok
            .as_ref()
            .map(|text| format!("<div class=\"flash ok\">{}</div>", h(text)))
            .unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{} · tinycd</title><link rel=\"stylesheet\" href=\"/static/style.css\"></head>\
         <body data-project=\"{}\" data-deployment=\"{}\">\
         <header><a class=\"brand\" href=\"/\">tinycd</a><nav>{nav}</nav>\
         <form method=\"post\" action=\"/logout\"><button class=\"quiet\" type=\"submit\">Log out</button></form></header>\
         <main>{flashes}{body}</main><script>{SCRIPT}</script></body></html>",
        h(title),
        h(project.unwrap_or("")),
        h(deployment.unwrap_or("")),
    )
}

fn bare(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{} · tinycd</title><link rel=\"stylesheet\" href=\"/static/style.css\"></head>\
         <body><main style=\"max-width:420px;margin-top:10vh\">{body}</main></body></html>",
        h(title)
    )
}

pub fn login(error: Option<&str>) -> String {
    let error = error
        .map(|text| format!("<div class=\"flash error\">{}</div>", h(text)))
        .unwrap_or_default();
    bare(
        "Log in",
        &format!(
            "<h1>tinycd</h1>{error}<form method=\"post\" action=\"/login\" class=\"card\">\
             <label>Password</label><input type=\"password\" name=\"password\" autofocus>\
             <p><button type=\"submit\">Log in</button></p></form>"
        ),
    )
}

pub fn setup(error: Option<&str>) -> String {
    let error = error
        .map(|text| format!("<div class=\"flash error\">{}</div>", h(text)))
        .unwrap_or_default();
    bare(
        "Set a password",
        &format!(
            "<h1>Welcome to tinycd</h1><p class=\"muted\">Choose the password for this dashboard.</p>{error}\
             <form method=\"post\" action=\"/setup\" class=\"card\">\
             <label>Password (8 characters or more)</label><input type=\"password\" name=\"password\" autofocus>\
             <label>Again</label><input type=\"password\" name=\"confirm\">\
             <p><button type=\"submit\">Save and continue</button></p></form>"
        ),
    )
}

fn pill(status: Status) -> String {
    format!(
        "<span class=\"pill {}\">{}</span>",
        status.label(),
        status.label()
    )
}

fn host_name<'a>(overview: &'a Overview, id: &'a str) -> &'a str {
    overview
        .hosts
        .iter()
        .find(|host| host.host.id == id)
        .map(|host| host.host.name.as_str())
        .unwrap_or(id)
}

pub fn overview(overview: &Overview, flash: &Flash) -> String {
    let mut body = String::from(
        "<div class=\"row\"><h1 style=\"margin:0\">Projects</h1><a class=\"button\" style=\"margin-left:auto\" href=\"/projects/new\">New project</a></div>",
    );
    if overview.projects.is_empty() {
        body.push_str("<div class=\"card\"><p>No projects yet. Add one here, or run <code>tinycd</code> inside a checkout.</p></div>");
    }
    body.push_str("<div class=\"grid\" style=\"margin-top:16px\">");
    for view in &overview.projects {
        let project = &view.project;
        let latest = view.deployments.first();
        let status = latest.map(|d| pill(d.status)).unwrap_or_default();
        let when = latest
            .map(|d| format!("{} ago", age_of(d.updated_at)))
            .unwrap_or_default();
        let running = if project.current.is_some() && !project.running {
            " · <span class=\"pill failed\">stopped</span>"
        } else {
            ""
        };
        let online = overview
            .hosts
            .iter()
            .any(|host| host.host.id == project.host && host.online);
        let host_pill = if online {
            ""
        } else {
            " <span class=\"pill offline\">host offline</span>"
        };
        body.push_str(&format!(
            "<div class=\"card\"><div class=\"row\"><a href=\"/projects/{id}\"><strong>{name}</strong></a>{status}{running}</div>\
             <div class=\"muted mono\" style=\"margin:6px 0\">{repo}</div>\
             <div class=\"muted\">{host} · {runtime}{host_pill} <span style=\"float:right\">{when}</span></div></div>",
            id = h(&project.id),
            name = h(&project.name),
            repo = h(&project.repo),
            host = h(host_name(overview, &project.host)),
            runtime = project.runtime.label(),
        ));
    }
    body.push_str("</div>");
    body.push_str("<h2>Hosts</h2><div class=\"row\">");
    for host in &overview.hosts {
        let state = if host.online { "online" } else { "offline" };
        body.push_str(&format!(
            "<span class=\"card\" style=\"margin:0\"><strong>{}</strong> <span class=\"pill {state}\">{state}</span> <span class=\"muted\">{}</span></span>",
            h(&host.host.name),
            h(&host.host.runtimes.iter().map(|r| r.label()).collect::<Vec<_>>().join(", ")),
        ));
    }
    body.push_str("</div><h2>Activity</h2><div class=\"card activity\" id=\"activity\"></div>");
    layout("Projects", "/", &body, None, None, flash)
}

/// The project form as the browser submits it.
#[derive(Deserialize, Default)]
pub struct ProjectForm {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub git_ref: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub install: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub check: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub dockerfile: String,
    #[serde(default)]
    pub sync: String,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub share: String,
    #[serde(default)]
    pub keep: String,
    #[serde(default)]
    pub poll: String,
    #[serde(default)]
    pub webhook_token: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub interlock: String,
}

fn blank(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

impl ProjectForm {
    pub fn into_project(self, existing: Option<&Project>) -> Result<Project, Error> {
        let mut env = BTreeMap::new();
        for line in self.env.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("environment line {line:?} must look like KEY=VALUE"))?;
            env.insert(key.trim().to_owned(), value.trim().to_owned());
        }
        let shell = blank(&self.shell).map(|shell| {
            shell
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let port = match blank(&self.port) {
            Some(port) => Some(
                port.parse::<u16>()
                    .map_err(|_| "the port must be a number")?,
            ),
            None => None,
        };
        let keep = match blank(&self.keep) {
            Some(keep) => keep.parse::<usize>().map_err(|_| "keep must be a number")?,
            None => existing.map(|p| p.keep).unwrap_or(5),
        };
        let poll = match blank(&self.poll) {
            Some(poll) if poll.eq_ignore_ascii_case("off") || poll == "0" => None,
            Some(poll) => Some(
                poll.parse::<u64>()
                    .map_err(|_| "poll must be a number of seconds, or off")?,
            ),
            None => None,
        };
        let runtime = match blank(&self.runtime) {
            Some(text) => {
                RuntimeKind::parse(&text).ok_or_else(|| format!("unknown runtime {text:?}"))?
            }
            None => RuntimeKind::Process,
        };
        let id = match blank(&self.id) {
            Some(id) => id,
            None => crate::util::slug(&self.name),
        };
        let mut project = existing.cloned().unwrap_or_default();
        project.id = id;
        project.name = blank(&self.name).unwrap_or_else(|| project.id.clone());
        project.repo = self.repo.trim().to_owned();
        project.git_ref = blank(&self.git_ref);
        project.host = blank(&self.host).unwrap_or_else(|| crate::model::LOCAL_HOST.to_owned());
        project.runtime = runtime;
        project.recipe = Recipe {
            shell,
            env,
            sync: blank(&self.sync),
            install: blank(&self.install),
            start: blank(&self.start),
            check: blank(&self.check),
            port,
            base: blank(&self.base),
            dockerfile: blank(&self.dockerfile),
        };
        project.share = self
            .share
            .split([',', ' ', '\n'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect();
        project.keep = keep;
        project.poll = poll;
        project.webhook_token = blank(&self.webhook_token);
        project.root = blank(&self.root).map(std::path::PathBuf::from);
        project.interlock = blank(&self.interlock).map(std::path::PathBuf::from);
        Ok(project)
    }
}

fn field(label: &str, name: &str, value: &str, hint: &str, mono: bool) -> String {
    format!(
        "<label>{}</label><input name=\"{name}\" value=\"{}\" placeholder=\"{}\"{}>",
        h(label),
        h(value),
        h(hint),
        if mono { " class=\"mono\"" } else { "" }
    )
}

pub fn project_form(overview: &Overview, project: &Project, new: bool, flash: &Flash) -> String {
    let title = if new {
        "New project"
    } else {
        "Project settings"
    };
    let body = format!(
        "<h1>{}</h1>{}",
        if new {
            "New project".to_owned()
        } else {
            format!("Settings: {}", h(&project.name))
        },
        form_fragment(overview, project, new)
    );
    layout(title, "/", &body, None, None, flash)
}

/// The project form on its own, for the new-project page and the settings
/// section of a project page.
fn form_fragment(overview: &Overview, project: &Project, new: bool) -> String {
    let hosts = overview
        .hosts
        .iter()
        .map(|host| {
            let selected = if host.host.id == project.host {
                " selected"
            } else {
                ""
            };
            let runtimes = host
                .host
                .runtimes
                .iter()
                .map(|r| r.label())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "<option value=\"{}\"{selected}>{} ({}{})</option>",
                h(&host.host.id),
                h(&host.host.name),
                if host.online { "online" } else { "offline" },
                if runtimes.is_empty() {
                    String::new()
                } else {
                    format!("; {runtimes}")
                }
            )
        })
        .collect::<String>();
    let runtime = |kind: RuntimeKind| {
        let selected = if project.runtime == kind {
            " selected"
        } else {
            ""
        };
        format!(
            "<option value=\"{}\"{selected}>{}</option>",
            kind.label(),
            match kind {
                RuntimeKind::Process => "process: run the start command directly",
                RuntimeKind::Oci => "container: build an image and run it with docker or podman",
            }
        )
    };
    let env = project
        .recipe
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let poll = project
        .poll
        .map(|p| p.to_string())
        .unwrap_or_else(|| "off".to_owned());
    let action = if new {
        "/projects".to_owned()
    } else {
        format!("/projects/{}", h(&project.id))
    };
    let id_field = if new {
        field(
            "Id (optional; derived from the name)",
            "id",
            &project.id,
            "my-app",
            true,
        )
    } else {
        format!(
            "<input type=\"hidden\" name=\"id\" value=\"{}\">",
            h(&project.id)
        )
    };
    format!(
        "<form method=\"post\" action=\"{action}\" class=\"card\">\
         <div class=\"two\"><div>{name}</div><div>{id_field}</div></div>\
         {repo}\
         <div class=\"two\"><div>{git_ref}</div><div><label>Host</label><select name=\"host\">{hosts}</select></div></div>\
         <div class=\"two\"><div><label>Runtime</label><select name=\"runtime\">{process}{oci}</select></div><div>{port}</div></div>\
         <h2>Recipe</h2><p class=\"muted\">Leave blank to use the repository's <code>.tinycd/config.toml</code>. Anything set here overrides it.</p>\
         {install}{start}{check}\
         <details><summary>Advanced</summary>\
         <div class=\"two\"><div>{base}</div><div>{dockerfile}</div></div>\
         <div class=\"two\"><div>{shell}</div><div>{sync}</div></div>\
         <label>Environment (KEY=VALUE per line)</label><textarea name=\"env\" rows=\"4\">{env}</textarea>\
         <div class=\"two\"><div>{share}</div><div>{keep}</div></div>\
         <div class=\"two\"><div>{poll}</div><div>{webhook}</div></div>\
         <div class=\"two\"><div>{root}</div><div>{interlock}</div></div>\
         </details>\
         <p style=\"margin-top:18px\"><button type=\"submit\">{submit}</button> <a class=\"button quiet\" href=\"{back}\">Cancel</a></p></form>",
        name = field("Name", "name", &project.name, "My app", false),
        repo = field(
            "Repository (Git URL)",
            "repo",
            &project.repo,
            "git@github.com:me/app.git",
            true
        ),
        git_ref = field(
            "Branch or tag (blank: the remote HEAD)",
            "git_ref",
            project.git_ref.as_deref().unwrap_or(""),
            "main",
            true
        ),
        process = runtime(RuntimeKind::Process),
        oci = runtime(RuntimeKind::Oci),
        port = field(
            "Port the app listens on",
            "port",
            &project
                .recipe
                .port
                .map(|p| p.to_string())
                .unwrap_or_default(),
            "3000",
            true
        ),
        install = field(
            "Install command",
            "install",
            project.recipe.install.as_deref().unwrap_or(""),
            "npm ci",
            true
        ),
        start = field(
            "Start command",
            "start",
            project.recipe.start.as_deref().unwrap_or(""),
            "npm start",
            true
        ),
        check = field(
            "Check command (must exit 0 before the release goes live)",
            "check",
            project.recipe.check.as_deref().unwrap_or(""),
            "curl -sf localhost:3000/health",
            true
        ),
        base = field(
            "Base image (container runtime)",
            "base",
            project.recipe.base.as_deref().unwrap_or(""),
            "node:22-slim",
            true
        ),
        dockerfile = field(
            "Dockerfile in the repository",
            "dockerfile",
            project.recipe.dockerfile.as_deref().unwrap_or(""),
            "Dockerfile",
            true
        ),
        shell = field(
            "Shell",
            "shell",
            &project
                .recipe
                .shell
                .as_ref()
                .map(|s| s.join(" "))
                .unwrap_or_default(),
            "sh -c",
            true
        ),
        sync = field(
            "Sync command",
            "sync",
            project.recipe.sync.as_deref().unwrap_or(""),
            "git clone --depth 1 \"$TINYCD_REPO\" .",
            true
        ),
        share = field(
            "Shared paths (kept across releases)",
            "share",
            &project.share.join(", "),
            "data/, .env",
            true
        ),
        keep = field(
            "Releases to keep",
            "keep",
            &project.keep.to_string(),
            "5",
            true
        ),
        poll = field("Poll every N seconds (or off)", "poll", &poll, "30", true),
        webhook = field(
            "Webhook token (for /api/hooks/<id>)",
            "webhook_token",
            project.webhook_token.as_deref().unwrap_or(""),
            "",
            true
        ),
        root = field(
            "Root for releases (local host only)",
            "root",
            &project
                .root
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            "",
            true
        ),
        interlock = field(
            "Interlock file",
            "interlock",
            &project
                .interlock
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            "",
            true
        ),
        submit = if new { "Create and deploy" } else { "Save" },
        back = if new {
            "/".to_owned()
        } else {
            format!("/projects/{}", h(&project.id))
        },
    )
}

pub fn project(
    overview: &Overview,
    view: &ProjectView,
    focus: Option<&str>,
    log: &str,
    settings: &Settings,
    hub_url: &str,
    flash: &Flash,
) -> String {
    let project = &view.project;
    let id = h(&project.id);
    let mut rows = String::new();
    for d in &view.deployments {
        let current = if project.current.as_deref() == Some(d.id.as_str()) {
            " <span class=\"pill\">current</span>"
        } else {
            ""
        };
        let rollback = if d.retained && matches!(d.status, Status::Retired) {
            format!(
                "<form class=\"inline\" method=\"post\" action=\"/projects/{id}/rollback\"><input type=\"hidden\" name=\"to\" value=\"{}\"><button class=\"quiet\" type=\"submit\">Roll back to</button></form>",
                h(&d.id)
            )
        } else {
            String::new()
        };
        let commit = if d.commit.is_empty() {
            String::new()
        } else {
            h(&d.commit[..d.commit.len().min(10)])
        };
        let error = d
            .error
            .as_deref()
            .map(|e| format!("<div class=\"muted\">{}</div>", h(e)))
            .unwrap_or_default();
        rows.push_str(&format!(
            "<tr><td><a class=\"mono\" href=\"/projects/{id}/deployments/{dep}\">{dep}</a>{current}</td><td>{status}{error}</td><td class=\"mono\">{commit}</td><td>{trigger}</td><td class=\"muted\">{when} ago</td><td>{rollback}</td></tr>",
            dep = h(&d.id),
            status = pill(d.status),
            trigger = h(&d.trigger),
            when = age_of(d.updated_at),
        ));
    }
    if rows.is_empty() {
        rows.push_str("<tr><td colspan=\"6\" class=\"muted\">Nothing deployed yet.</td></tr>");
    }
    let running = match (&project.current, project.running) {
        (Some(_), true) => "<span class=\"pill live\">running</span>",
        (Some(_), false) => "<span class=\"pill failed\">stopped</span>",
        (None, _) => "",
    };
    let github = match &settings.github {
        Some(app) if crate::git::github_repo(&project.repo).is_some() => format!(
            "<p class=\"muted\">GitHub App <a href=\"{}\">{}</a> covers this repository if it is installed there; pushes to {} deploy.</p>",
            h(&github::install_url(app)),
            h(&app.name),
            h(project.git_ref.as_deref().unwrap_or("the default branch")),
        ),
        _ => String::new(),
    };
    let hook = match &project.webhook_token {
        Some(_) => format!(
            "<p class=\"muted\">Webhook: <code>POST {}/api/hooks/{id}</code> with the project token as a bearer, GitLab, or GitHub secret.</p>",
            h(settings.public_url.as_deref().unwrap_or(hub_url))
        ),
        None => String::new(),
    };
    let focus_title = focus
        .map(|f| format!("Log of {}", h(f)))
        .unwrap_or_else(|| "Log".to_owned());
    let body = format!(
        "<div class=\"row\"><h1 style=\"margin:0\">{name}</h1>{running}\
         <span style=\"margin-left:auto\"></span>\
         <form class=\"inline\" method=\"post\" action=\"/projects/{id}/deploy\"><button type=\"submit\">Deploy now</button></form>\
         <form class=\"inline\" method=\"post\" action=\"/projects/{id}/rollback\"><button class=\"quiet\" type=\"submit\">Roll back</button></form>\
         <form class=\"inline\" method=\"post\" action=\"/projects/{id}/restart\"><button class=\"quiet\" type=\"submit\">Restart</button></form>\
         <form class=\"inline\" method=\"post\" action=\"/projects/{id}/stop\"><button class=\"quiet\" type=\"submit\">Stop</button></form>\
         </div>\
         <div class=\"muted mono\" style=\"margin:8px 0 16px\">{repo}{git_ref} · {host} · {runtime}{port}</div>\
         {github}{hook}\
         <h2>{focus_title}</h2><pre class=\"log\" id=\"log\">{log}</pre>\
         <h2>Deployments</h2><div class=\"card\"><table><tr><th>Release</th><th>Status</th><th>Commit</th><th>Trigger</th><th>Updated</th><th></th></tr>{rows}</table></div>\
         <h2>Activity</h2><div class=\"card activity\" id=\"activity\"></div>\
         <details style=\"margin-top:24px\"><summary>Settings</summary>{form}</details>\
         <form method=\"post\" action=\"/projects/{id}/delete\" style=\"margin-top:24px\" onsubmit=\"return confirm('Remove this project and stop it?')\"><button class=\"danger\" type=\"submit\">Remove project</button></form>",
        name = h(&project.name),
        repo = h(&project.repo),
        git_ref = project
            .git_ref
            .as_deref()
            .map(|r| format!(" @ {}", h(r)))
            .unwrap_or_default(),
        host = h(host_name(overview, &project.host)),
        runtime = project.runtime.label(),
        port = project
            .recipe
            .port
            .map(|p| format!(" · port {p}"))
            .unwrap_or_default(),
        log = h(log),
        form = form_fragment(overview, project, false),
    );
    layout(&project.name, "/", &body, Some(&project.id), focus, flash)
}

pub fn deployment(view: &ProjectView, record: &Deployment, log: &str) -> String {
    let body = format!(
        "<p><a href=\"/projects/{id}\">← {name}</a></p><h1>Release {dep} {status}</h1>\
         <p class=\"muted mono\">{commit} · {trigger} · created {created} ago{error}</p>\
         <pre class=\"log\" id=\"log\">{log}</pre>",
        id = h(&view.project.id),
        name = h(&view.project.name),
        dep = h(&record.id),
        status = pill(record.status),
        commit = h(&record.commit),
        trigger = h(&record.trigger),
        created = age_of(record.created_at),
        error = record
            .error
            .as_deref()
            .map(|e| format!(" · {}", h(e)))
            .unwrap_or_default(),
        log = h(log),
    );
    layout(
        &format!("Release {}", record.id),
        "/",
        &body,
        Some(&view.project.id),
        Some(&record.id),
        &Flash::default(),
    )
}

pub fn hosts(overview: &Overview, token: Option<(&str, &str, &str)>, flash: &Flash) -> String {
    let mut rows = String::new();
    for host in &overview.hosts {
        let state = if host.online { "online" } else { "offline" };
        let seen = host
            .host
            .last_seen
            .map(|t| format!("{} ago", age_of(t)))
            .unwrap_or_else(|| "never".to_owned());
        let projects = overview
            .projects
            .iter()
            .filter(|p| p.project.host == host.host.id)
            .count();
        rows.push_str(&format!(
            "<tr><td><strong>{}</strong><div class=\"muted mono\">{}</div></td><td><span class=\"pill {state}\">{state}</span></td><td>{}</td><td>{}</td><td>{} {}</td><td class=\"muted\">{seen}</td></tr>",
            h(&host.host.name),
            h(&host.host.id),
            h(&host.host.runtimes.iter().map(|r| r.label()).collect::<Vec<_>>().join(", ")),
            projects,
            h(&host.host.os),
            h(&host.host.version),
        ));
    }
    let issued = token
        .map(|(name, token, url)| {
            format!(
                "<div class=\"card\"><h2 style=\"margin-top:0\">Join {name}</h2>\
                 <p>On the new machine, install tinycd and run this once. The token is valid for one day and carries this dashboard's certificate fingerprint, so the link is verified end to end.</p>\
                 <pre class=\"log token\">tinycd agent {url} --token {token}</pre>\
                 <p class=\"muted\">The agent keeps its credentials in its data directory and reconnects on its own; install it as a service with <code>tinycd service install --agent</code>.</p></div>",
                name = h(name),
                url = h(url),
                token = h(token)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<h1>Hosts</h1>{issued}<div class=\"card\"><table><tr><th>Host</th><th>State</th><th>Runtimes</th><th>Projects</th><th>System</th><th>Last seen</th></tr>{rows}</table></div>\
         <form method=\"post\" action=\"/hosts\" class=\"card\"><h2 style=\"margin-top:0\">Add a host</h2>\
         <p class=\"muted\">Another machine that runs cartons. It connects out to this dashboard, so it can sit behind NAT.</p>\
         <label>Name</label><input name=\"name\" placeholder=\"pi-in-the-closet\"><p><button type=\"submit\">Create join token</button></p></form>"
    );
    layout("Hosts", "/hosts", &body, None, None, flash)
}

pub fn settings(
    overview: &Overview,
    settings: &Settings,
    fingerprint: &str,
    flash: &Flash,
) -> String {
    let github = match &settings.github {
        Some(app) => format!(
            "<p>Connected as <a href=\"{}\">{}</a> (owner {}, app id {}).</p>\
             <p><a class=\"button\" href=\"{}\">Install on repositories</a> \
             <form class=\"inline\" method=\"post\" action=\"/github/disconnect\"><button class=\"quiet\" type=\"submit\">Disconnect</button></form></p>\
             <p class=\"muted\">Pushes deploy through the webhook when a public URL is set and the webhook is active on GitHub. If you created the app without a public URL, enable Active in its GitHub webhook settings. Polling also works for private repositories.</p>",
            h(&app.html_url),
            h(&app.name),
            h(&app.owner),
            app.id,
            h(&github::install_url(app)),
        ),
        None => "<p>Create a GitHub App owned by you. GitHub sends you back here with its credentials; nothing goes through a third party.</p>\
                 <form method=\"get\" action=\"/github/connect\"><div class=\"two\"><div><label>App name</label><input name=\"name\" placeholder=\"tinycd-myhost\"></div>\
                 <div><label>Organisation (blank: your account)</label><input name=\"org\"></div></div>\
                 <p><button type=\"submit\">Create GitHub App</button></p></form>".to_owned(),
    };
    let body = format!(
        "<h1>Settings</h1>\
         <form method=\"post\" action=\"/settings\" class=\"card\"><h2 style=\"margin-top:0\">Dashboard</h2>\
         {public_url}\
         <p class=\"muted\">Where GitHub and other machines reach this dashboard, such as <code>https://deploy.example.com</code>. Put a reverse proxy with a real certificate in front for that; agents on other machines can use the built-in HTTPS listener directly.</p>\
         {poll}\
         <div class=\"two\"><div><label>New password</label><input type=\"password\" name=\"password\"></div><div><label>Again</label><input type=\"password\" name=\"confirm\"></div></div>\
         <p><button type=\"submit\">Save</button></p></form>\
         <div class=\"card\"><h2 style=\"margin-top:0\">GitHub</h2>{github}</div>\
         <div class=\"card\"><h2 style=\"margin-top:0\">This dashboard</h2>\
         <p class=\"muted\">tinycd {version} · up {up} · {hosts} hosts · {projects} projects</p>\
         <p class=\"muted\">Secure listener: {secure}<br>Certificate fingerprint: <code>{fingerprint}</code></p></div>",
        public_url = field(
            "Public URL",
            "public_url",
            settings.public_url.as_deref().unwrap_or(""),
            "https://deploy.example.com",
            true
        ),
        poll = field(
            "Default poll interval for new projects (seconds)",
            "poll",
            &settings.poll.to_string(),
            "30",
            true
        ),
        version = h(&overview.version),
        up = age_of(overview.started_at),
        hosts = overview.hosts.len(),
        projects = overview.projects.len(),
        secure = settings
            .secure_bind
            .as_deref()
            .map(h)
            .unwrap_or_else(|| "off until a host is added".to_owned()),
        fingerprint = h(fingerprint),
    );
    layout("Settings", "/settings", &body, None, None, flash)
}

pub fn github_connect(action: &str, manifest: &str) -> String {
    bare(
        "Connecting to GitHub",
        &format!(
            "<h1>Creating your GitHub App</h1><p class=\"muted\">Taking you to GitHub. Review the app and press <em>Create GitHub App</em>; GitHub sends you back here with its credentials.</p>\
             <form id=\"manifest\" method=\"post\" action=\"{}\"><input type=\"hidden\" name=\"manifest\" value=\"{}\"><button type=\"submit\">Continue to GitHub</button></form>\
             <script>document.getElementById('manifest').submit();</script>",
            h(action),
            h(manifest)
        ),
    )
}
