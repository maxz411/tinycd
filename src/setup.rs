//! Interactive setup: a small full-screen form engine, and the two wizards
//! built on it. The project wizard writes .tinycd/config.toml for a
//! checkout that has none; the node wizard sets this machine up as a
//! dashboard with a password, a service, and optionally a GitHub App.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

use crate::{model::RuntimeKind, util::Error};

// ----- the form engine -----------------------------------------------------

pub enum Kind {
    Intro,
    Text {
        value: String,
        secret: bool,
        required: bool,
    },
    Choice {
        options: Vec<(String, String)>,
        selected: usize,
    },
    Summary,
}

pub struct Step {
    pub key: &'static str,
    pub title: &'static str,
    pub help: String,
    pub kind: Kind,
    /// Validation of a text value; returns an error to show.
    pub validate: Option<fn(&str) -> Option<String>>,
}

impl Step {
    pub fn intro(title: &'static str, help: impl Into<String>) -> Self {
        Self {
            key: "",
            title,
            help: help.into(),
            kind: Kind::Intro,
            validate: None,
        }
    }

    pub fn text(
        key: &'static str,
        title: &'static str,
        help: impl Into<String>,
        value: impl Into<String>,
        required: bool,
    ) -> Self {
        Self {
            key,
            title,
            help: help.into(),
            kind: Kind::Text {
                value: value.into(),
                secret: false,
                required,
            },
            validate: None,
        }
    }

    pub fn secret(key: &'static str, title: &'static str, help: impl Into<String>) -> Self {
        Self {
            key,
            title,
            help: help.into(),
            kind: Kind::Text {
                value: String::new(),
                secret: true,
                required: true,
            },
            validate: None,
        }
    }

    pub fn choice(
        key: &'static str,
        title: &'static str,
        help: impl Into<String>,
        options: Vec<(&str, &str)>,
        selected: usize,
    ) -> Self {
        Self {
            key,
            title,
            help: help.into(),
            kind: Kind::Choice {
                options: options
                    .into_iter()
                    .map(|(a, b)| (a.to_owned(), b.to_owned()))
                    .collect(),
                selected,
            },
            validate: None,
        }
    }

    pub fn summary(title: &'static str, help: impl Into<String>) -> Self {
        Self {
            key: "",
            title,
            help: help.into(),
            kind: Kind::Summary,
            validate: None,
        }
    }

    pub fn validated(mut self, validate: fn(&str) -> Option<String>) -> Self {
        self.validate = Some(validate);
        self
    }
}

/// What a form collected: text values, and the chosen option's label for
/// choices.
#[derive(Default, Debug)]
pub struct Answers {
    values: HashMap<&'static str, String>,
}

impl Answers {
    pub fn get(&self, key: &str) -> &str {
        self.values.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn optional(&self, key: &str) -> Option<String> {
        let value = self.get(key).trim();
        (!value.is_empty()).then(|| value.to_owned())
    }
}

type Summarize = Box<dyn Fn(&Answers) -> Vec<String>>;

struct Form {
    name: String,
    steps: Vec<Step>,
    index: usize,
    error: Option<String>,
    summarize: Summarize,
}

enum Flow {
    Stay,
    Cancel,
    Done,
}

/// Run a form to completion. Returns None when the user cancels.
pub fn run_form(
    name: &str,
    steps: Vec<Step>,
    summarize: impl Fn(&Answers) -> Vec<String> + 'static,
) -> Result<Option<Answers>, Error> {
    let mut form = Form {
        name: name.to_owned(),
        steps,
        index: 0,
        error: None,
        summarize: Box::new(summarize),
    };
    let mut terminal = ratatui::init();
    let result = (|| loop {
        terminal.draw(|frame| form.render(frame))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match form.handle(key) {
                Flow::Stay => {}
                Flow::Cancel => return Ok(None),
                Flow::Done => return Ok(Some(form.answers())),
            }
        }
    })();
    ratatui::restore();
    result
}

impl Form {
    fn answers(&self) -> Answers {
        let mut values = HashMap::new();
        for step in &self.steps {
            match &step.kind {
                Kind::Text { value, .. } => {
                    values.insert(step.key, value.trim().to_owned());
                }
                Kind::Choice { options, selected } => {
                    values.insert(step.key, options[*selected].0.clone());
                }
                _ => {}
            }
        }
        Answers { values }
    }

    fn handle(&mut self, key: KeyEvent) -> Flow {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Flow::Cancel;
        }
        self.error = None;
        let last = self.index + 1 == self.steps.len();
        let step = &mut self.steps[self.index];
        match &mut step.kind {
            Kind::Intro => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => self.advance(),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => return Flow::Cancel,
                _ => {}
            },
            Kind::Text {
                value, required, ..
            } => match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => value.push(c),
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Enter => {
                    if *required && value.trim().is_empty() {
                        self.error = Some("a value is required".to_owned());
                    } else if let Some(error) = step.validate.and_then(|check| check(value.trim()))
                    {
                        self.error = Some(error);
                    } else if last {
                        return Flow::Done;
                    } else {
                        self.advance();
                    }
                }
                KeyCode::Esc => self.back(),
                _ => {}
            },
            Kind::Choice { options, selected } => match key.code {
                KeyCode::Left | KeyCode::Up => {
                    *selected = (*selected + options.len() - 1) % options.len();
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Char(' ') | KeyCode::Tab => {
                    *selected = (*selected + 1) % options.len();
                }
                KeyCode::Enter => {
                    if last {
                        return Flow::Done;
                    }
                    self.advance();
                }
                KeyCode::Esc => self.back(),
                _ => {}
            },
            Kind::Summary => match key.code {
                KeyCode::Enter => return Flow::Done,
                KeyCode::Esc => self.back(),
                _ => {}
            },
        }
        Flow::Stay
    }

    fn advance(&mut self) {
        if self.index + 1 < self.steps.len() {
            self.index += 1;
        }
    }

    fn back(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    fn render(&self, frame: &mut Frame) {
        let step = &self.steps[self.index];
        let mut lines: Vec<Line> = step
            .help
            .lines()
            .map(|line| Line::from(line.to_owned()))
            .collect();
        let cursor = Span::styled("▌", Style::default().fg(Color::Cyan));
        match &step.kind {
            Kind::Text { value, secret, .. } => {
                lines.push(Line::default());
                let shown = if *secret {
                    "•".repeat(value.chars().count())
                } else {
                    value.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan)),
                    Span::raw(shown),
                    cursor,
                ]));
            }
            Kind::Choice { options, selected } => {
                lines.push(Line::default());
                for (index, (label, detail)) in options.iter().enumerate() {
                    let chosen = index == *selected;
                    let marker = if chosen { "●" } else { "○" };
                    let style = if chosen {
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    lines.push(Line::styled(
                        format!("  {marker} {label} — {detail}"),
                        style,
                    ));
                }
            }
            Kind::Summary => {
                lines.push(Line::default());
                for line in (self.summarize)(&self.answers()) {
                    lines.push(Line::styled(
                        format!("  {line}"),
                        Style::default().fg(Color::Cyan),
                    ));
                }
            }
            Kind::Intro => {}
        }
        if let Some(error) = &self.error {
            lines.push(Line::default());
            lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
        }

        let keys = match step.kind {
            Kind::Intro => " Enter continue · Esc quit ",
            Kind::Choice { .. } => " Space switch · Enter continue · Esc back ",
            Kind::Summary => " Enter confirm · Esc back ",
            Kind::Text { .. } => " Enter continue · Esc back · Ctrl-C quit ",
        };
        let block = Block::bordered()
            .title(
                Line::from(format!(
                    " {} — {} ({}/{}) ",
                    self.name,
                    step.title,
                    self.index + 1,
                    self.steps.len()
                ))
                .centered(),
            )
            .title_bottom(Line::from(keys).centered());
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            frame.area(),
        );
    }
}

// ----- the project wizard --------------------------------------------------

/// What the project wizard collected. `repo` is None when the checkout's
/// origin should keep being used, so moving the remote later needs no
/// config edit.
pub struct ProjectAnswers {
    pub repo: Option<String>,
    pub runtime: RuntimeKind,
    pub port: Option<u16>,
    pub install: Option<String>,
    pub start: String,
    pub local_state: bool,
}

/// Run the project wizard. Returns None when the user cancels.
pub fn project_wizard(
    project: &Path,
    origin: Option<&str>,
    container_runtime: Option<&str>,
) -> Result<Option<ProjectAnswers>, Error> {
    let (install, start, port) = suggest(project);
    let repo_help = match origin {
        Some(_) => {
            "Git remote to watch, detected from this checkout's origin.\n\
             Leave it as is, or replace it with another URL or remote name.\n\
             (Leaving it unchanged tracks origin even if its URL changes later.)"
        }
        None => {
            "Git URL or remote name to watch. This checkout has no origin, so a value is required."
        }
    };
    let mut steps = vec![
        Step::intro(
            "welcome",
            format!(
                "No tinycd configuration was found in\n{}\n\n\
                 This wizard writes .tinycd/config.toml — the file tinycd reads — and \
                 leaves the rest of your working tree untouched. Deployments run from \
                 fresh clones of the remote, so you keep committing and pushing here as \
                 usual, and every push deploys.",
                project.display()
            ),
        ),
        Step::text(
            "repo",
            "remote",
            repo_help,
            origin.unwrap_or_default(),
            origin.is_none(),
        ),
    ];
    match container_runtime {
        Some(cli) => steps.push(Step::choice(
            "runtime",
            "runtime",
            format!("How should releases run? {cli} is available on this machine."),
            vec![
                (
                    "process",
                    "run the start command directly, as tinycd always has",
                ),
                (
                    "container",
                    "build an image from the recipe and run it in a container",
                ),
            ],
            0,
        )),
        None => steps.push(Step::choice(
            "runtime",
            "runtime",
            "No container runtime (docker, podman, nerdctl) answers on this machine, so releases \
             run as processes. Install one later and switch the project in the dashboard.",
            vec![("process", "run the start command directly")],
            0,
        )),
    }
    steps.push(
        Step::text(
            "port",
            "port",
            "The port the application listens on. Containers publish it, and every release \
             receives it as PORT. Leave empty if the application does not listen.",
            port,
            false,
        )
        .validated(|value| {
            (!value.is_empty() && value.parse::<u16>().is_err())
                .then(|| "the port must be a number between 1 and 65535".to_owned())
        }),
    );
    steps.push(Step::text(
        "install",
        "install command",
        "Install command, run in each fresh release before it starts.\nLeave empty for none.",
        install,
        false,
    ));
    steps.push(Step::text(
        "start",
        "start command",
        "Start command (required). It runs in the release folder in the foreground; \
         tinycd restarts it on every deployment.",
        start,
        true,
    ));
    steps.push(Step::choice(
        "state",
        "releases",
        "Where should releases live?",
        vec![
            (
                "home",
                "~/.tinycd/<name>-<hash>: outside the repo; the working tree stays clean",
            ),
            (
                "local",
                ".tinycd/ inside this repo: self-contained; releases are gitignored",
            ),
        ],
        0,
    ));
    steps.push(Step::summary(
        "summary",
        ".tinycd/config.toml will contain the lines below.\n\n\
         Enter saves the file and starts tinycd. Commit .tinycd/config.toml to share the \
         deployment recipe.",
    ));

    let origin_owned = origin.map(str::to_owned);
    let Some(answers) = run_form("tinycd setup", steps, move |answers| {
        config_toml(&project_answers(answers, origin_owned.as_deref()))
            .lines()
            .map(str::to_owned)
            .collect()
    })?
    else {
        return Ok(None);
    };
    Ok(Some(project_answers(&answers, origin)))
}

fn project_answers(answers: &Answers, origin: Option<&str>) -> ProjectAnswers {
    let repo = answers.get("repo").trim();
    ProjectAnswers {
        repo: Some(repo.to_owned())
            .filter(|repo| !repo.is_empty() && Some(repo.as_str()) != origin),
        runtime: RuntimeKind::parse(answers.get("runtime")).unwrap_or(RuntimeKind::Process),
        port: answers.get("port").trim().parse().ok(),
        install: answers.optional("install"),
        start: answers.get("start").trim().to_owned(),
        local_state: answers.get("state") == "local",
    }
}

/// Write the collected answers as .tinycd/config.toml. With local state, a
/// .gitignore keeps the config committable while releases stay untracked.
pub fn write_config(project: &Path, answers: &ProjectAnswers) -> Result<PathBuf, Error> {
    let dir = project.join(".tinycd");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;

    let path = dir.join("config.toml");
    std::fs::write(&path, config_toml(answers))
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    if answers.local_state {
        std::fs::write(dir.join(".gitignore"), "*\n!config.toml\n!.gitignore\n")
            .map_err(|error| format!("failed to write {}/.gitignore: {error}", dir.display()))?;
    }
    Ok(path)
}

fn config_toml(answers: &ProjectAnswers) -> String {
    let mut contents = String::from("# tinycd configuration: https://github.com/maxz411/tinycd\n");
    if let Some(repo) = &answers.repo {
        contents.push_str(&format!("repo = \"{}\"\n", toml_escape(repo)));
    }
    if answers.local_state {
        contents.push_str("root = \".\"\n");
    }
    if answers.runtime == RuntimeKind::Oci {
        contents.push_str("runtime = \"container\"\n");
    }
    if let Some(port) = answers.port {
        contents.push_str(&format!("port = {port}\n"));
    }
    if let Some(install) = &answers.install {
        contents.push_str(&format!("install = \"{}\"\n", toml_escape(install)));
    }
    contents.push_str(&format!("start = \"{}\"\n", toml_escape(&answers.start)));
    contents
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', "\\\"")
}

/// Prefill install, start, and port from what the project looks like.
fn suggest(project: &Path) -> (String, String, String) {
    let found = |file: &str| project.join(file).exists();
    if found("package.json") {
        ("npm ci".into(), "npm start".into(), "3000".into())
    } else if found("Cargo.toml") {
        (
            "cargo build --release".into(),
            "cargo run --release".into(),
            String::new(),
        )
    } else if found("go.mod") {
        ("go build -o app .".into(), "./app".into(), "8080".into())
    } else if found("requirements.txt") || found("pyproject.toml") {
        (
            "pip install -r requirements.txt".into(),
            String::new(),
            "8000".into(),
        )
    } else {
        (String::new(), String::new(), String::new())
    }
}

// ----- the node wizard ------------------------------------------------------

/// What `tinycd init` collected.
pub struct NodeAnswers {
    pub password: Option<String>,
    pub public_url: Option<String>,
    pub install_service: bool,
    pub connect_github: bool,
}

/// Run the node wizard: password, public URL, service, GitHub.
pub fn node_wizard(
    data: &Path,
    has_password: bool,
    hub_running: bool,
) -> Result<Option<NodeAnswers>, Error> {
    let mut steps = vec![Step::intro(
        "welcome",
        format!(
            "This sets up the tinycd dashboard on this machine.\n\n\
             Data directory: {}\n\n\
             The dashboard runs on this machine and can run projects here, in containers or as \
             processes, and on other machines that join it. Everything here can be changed \
             later in the dashboard's settings.",
            data.display()
        ),
    )];
    if !has_password {
        steps.push(
            Step::secret(
                "password",
                "password",
                "Choose the password for the web dashboard (8 characters or more).",
            )
            .validated(|value| (value.len() < 8).then(|| "at least 8 characters".to_owned())),
        );
    }
    steps.push(
        Step::text(
            "public_url",
            "public URL",
            "If GitHub or other machines should reach this dashboard, the URL they use, such as \
             https://deploy.example.com (put a reverse proxy with a certificate in front).\n\
             Leave empty for a dashboard that is only reached from this machine; polling still \
             deploys every push, and hosts on the local network can still join over the built-in \
             HTTPS listener.",
            "",
            false,
        )
        .validated(|value| {
            (!value.is_empty() && !value.starts_with("http://") && !value.starts_with("https://"))
                .then(|| "the URL must start with http:// or https://".to_owned())
        }),
    );
    steps.push(Step::choice(
        "service",
        "service",
        if hub_running {
            "Install tinycd as a service so the dashboard and its projects come back after a reboot? \
             (A dashboard is running right now; the service takes over after it stops.)"
        } else {
            "Install tinycd as a service so the dashboard and its projects come back after a reboot?"
        },
        vec![("yes", "install and start the service"), ("no", "I will run tinycd serve myself")],
        0,
    ));
    steps.push(Step::choice(
        "github",
        "GitHub",
        "Create a GitHub App for push-to-deploy, private repositories, and commit statuses? \
         GitHub's form opens in your browser and sends the app's credentials back to the \
         dashboard; you own the app.",
        vec![
            ("yes", "open the GitHub App form in the browser"),
            ("later", "skip; available in Settings"),
        ],
        0,
    ));
    steps.push(Step::summary("summary", "Enter applies these settings."));

    let Some(answers) = run_form("tinycd init", steps, |answers| {
        let mut lines = Vec::new();
        if !answers.get("password").is_empty() {
            lines.push("password: set".to_owned());
        }
        lines.push(format!(
            "public URL: {}",
            answers
                .optional("public_url")
                .unwrap_or_else(|| "none (local only)".to_owned())
        ));
        lines.push(format!("service: {}", answers.get("service")));
        lines.push(format!("GitHub App: {}", answers.get("github")));
        lines
    })?
    else {
        return Ok(None);
    };
    Ok(Some(NodeAnswers {
        password: answers.optional("password"),
        public_url: answers
            .optional("public_url")
            .map(|url| url.trim_end_matches('/').to_owned()),
        install_service: answers.get("service") == "yes",
        connect_github: answers.get("github") == "yes",
    }))
}

/// Open a URL in the user's browser, best effort.
pub fn open_browser(url: &str) -> bool {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
