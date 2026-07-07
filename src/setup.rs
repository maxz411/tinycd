//! Interactive first-run setup: a small full-screen wizard that writes
//! .tinycd/config.toml for a checkout that has none.

use std::path::{Path, PathBuf};

use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

use crate::Error;

/// What the wizard collected. `repo` is None when the checkout's origin
/// should keep being used, so moving the remote later needs no config edit.
pub struct Answers {
    pub repo: Option<String>,
    pub install: Option<String>,
    pub start: String,
    pub local_state: bool,
}

/// Run the wizard. Returns None when the user cancels.
pub fn run(project: &Path, origin: Option<&str>) -> Result<Option<Answers>, Error> {
    let (install, start) = suggest(project);
    let mut wizard = Wizard {
        step: Step::Intro,
        project: project.display().to_string(),
        origin: origin.map(str::to_owned),
        repo: origin.unwrap_or_default().to_owned(),
        install,
        start,
        local_state: false,
        error: None,
    };

    let mut terminal = ratatui::init();
    let result = (|| loop {
        terminal.draw(|frame| wizard.render(frame))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match wizard.handle(key) {
                Flow::Stay => {}
                Flow::Cancel => return Ok(None),
                Flow::Done(answers) => return Ok(Some(answers)),
            }
        }
    })();
    ratatui::restore();
    result
}

/// Write the collected answers as .tinycd/config.toml. With local state, a
/// .gitignore keeps the config committable while releases stay untracked.
pub fn write_config(project: &Path, answers: &Answers) -> Result<PathBuf, Error> {
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

fn config_toml(answers: &Answers) -> String {
    let mut contents = String::from("# tinycd configuration: https://github.com/maxz411/tinycd\n");
    if let Some(repo) = &answers.repo {
        contents.push_str(&format!("repo = \"{}\"\n", toml_escape(repo)));
    }
    if answers.local_state {
        contents.push_str("root = \".\"\n");
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

/// Prefill install and start commands from what the project looks like.
fn suggest(project: &Path) -> (String, String) {
    let found = |file: &str| project.join(file).exists();
    if found("package.json") {
        ("npm ci".into(), "npm start".into())
    } else if found("Cargo.toml") {
        ("cargo build --release".into(), "cargo run --release".into())
    } else if found("go.mod") {
        ("go build -o app .".into(), "./app".into())
    } else if found("requirements.txt") {
        ("pip install -r requirements.txt".into(), String::new())
    } else {
        (String::new(), String::new())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Intro,
    Repo,
    Install,
    Start,
    State,
    Summary,
}

enum Flow {
    Stay,
    Cancel,
    Done(Answers),
}

struct Wizard {
    step: Step,
    project: String,
    origin: Option<String>,
    repo: String,
    install: String,
    start: String,
    local_state: bool,
    error: Option<String>,
}

impl Wizard {
    fn handle(&mut self, key: KeyEvent) -> Flow {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Flow::Cancel;
        }
        self.error = None;

        match self.step {
            Step::Intro => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => self.step = Step::Repo,
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => return Flow::Cancel,
                _ => {}
            },
            Step::Repo | Step::Install | Step::Start => match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                    self.field().push(c);
                }
                KeyCode::Backspace => {
                    self.field().pop();
                }
                KeyCode::Enter => self.advance(),
                KeyCode::Esc => self.back(),
                _ => {}
            },
            Step::State => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char(' ')
                | KeyCode::Tab => self.local_state = !self.local_state,
                KeyCode::Enter => self.step = Step::Summary,
                KeyCode::Esc => self.back(),
                _ => {}
            },
            Step::Summary => match key.code {
                KeyCode::Enter => return Flow::Done(self.answers()),
                KeyCode::Esc => self.back(),
                _ => {}
            },
        }
        Flow::Stay
    }

    fn field(&mut self) -> &mut String {
        match self.step {
            Step::Repo => &mut self.repo,
            Step::Install => &mut self.install,
            _ => &mut self.start,
        }
    }

    fn advance(&mut self) {
        match self.step {
            Step::Repo if self.repo.trim().is_empty() && self.origin.is_none() => {
                self.error = Some(
                    "a Git URL or remote name is required: this checkout has no origin".into(),
                );
            }
            Step::Repo => self.step = Step::Install,
            Step::Install => self.step = Step::Start,
            Step::Start if self.start.trim().is_empty() => {
                self.error = Some("a start command is required".into());
            }
            Step::Start => self.step = Step::State,
            _ => {}
        }
    }

    fn back(&mut self) {
        self.step = match self.step {
            Step::Intro | Step::Repo => Step::Intro,
            Step::Install => Step::Repo,
            Step::Start => Step::Install,
            Step::State => Step::Start,
            Step::Summary => Step::State,
        };
    }

    fn answers(&self) -> Answers {
        let repo = self.repo.trim();
        Answers {
            repo: Some(repo.to_owned())
                .filter(|repo| !repo.is_empty() && Some(repo.as_str()) != self.origin.as_deref()),
            install: Some(self.install.trim().to_owned()).filter(|install| !install.is_empty()),
            start: self.start.trim().to_owned(),
            local_state: self.local_state,
        }
    }

    fn render(&self, frame: &mut Frame) {
        let (title, mut lines) = self.content();
        if let Some(error) = &self.error {
            lines.push(Line::default());
            lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
        }

        let keys = match self.step {
            Step::Intro => " Enter set up · Esc quit ",
            Step::State => " Space switch · Enter continue · Esc back ",
            Step::Summary => " Enter save and start · Esc back ",
            _ => " Enter continue · Esc back · Ctrl-C quit ",
        };
        let block = Block::bordered()
            .title(Line::from(format!(" tinycd setup — {title} ")).centered())
            .title_bottom(Line::from(keys).centered());
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            frame.area(),
        );
    }

    fn content(&self) -> (&'static str, Vec<Line<'_>>) {
        let input = |value: &str| {
            Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan)),
                Span::raw(value.to_owned()),
                Span::styled("▌", Style::default().fg(Color::Cyan)),
            ])
        };
        let text = |content: &str| {
            content
                .lines()
                .map(|line| Line::from(line.to_owned()))
                .collect::<Vec<_>>()
        };

        match self.step {
            Step::Intro => (
                "welcome",
                text(&format!(
                    "No tinycd configuration was found in\n{}\n\n\
                     This wizard writes .tinycd/config.toml — the file tinycd reads — and \
                     leaves the rest of your working tree untouched. Deployments run from \
                     fresh clones of the remote, so you keep committing and pushing here as \
                     usual, and every push deploys.",
                    self.project
                )),
            ),
            Step::Repo => {
                let mut lines = text(match &self.origin {
                    Some(_) => {
                        "Git remote to watch, detected from this checkout's origin.\n\
                                Leave it as is, or replace it with another URL or remote name.\n\
                                (Leaving it unchanged tracks origin even if its URL changes later.)"
                    }
                    None => {
                        "Git URL or remote name to watch. This checkout has no origin, \
                             so a value is required."
                    }
                });
                lines.push(Line::default());
                lines.push(input(&self.repo));
                ("remote", lines)
            }
            Step::Install => {
                let mut lines = text(
                    "Install command, run in each fresh release before it starts.\n\
                     Leave empty for none.",
                );
                lines.push(Line::default());
                lines.push(input(&self.install));
                ("install command", lines)
            }
            Step::Start => {
                let mut lines = text(
                    "Start command (required). It runs in the release folder in the \
                     foreground; tinycd restarts it on every deployment.",
                );
                lines.push(Line::default());
                lines.push(input(&self.start));
                ("start command", lines)
            }
            Step::State => {
                let option = |selected: bool, label: &str, detail: &str| {
                    let marker = if selected { "●" } else { "○" };
                    let style = if selected {
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    Line::styled(format!("  {marker} {label} — {detail}"), style)
                };
                let mut lines = text("Where should releases live?");
                lines.push(Line::default());
                lines.push(option(
                    !self.local_state,
                    "~/.tinycd/<name>-<hash>",
                    "outside the repo; the working tree stays clean",
                ));
                lines.push(option(
                    self.local_state,
                    ".tinycd/ inside this repo",
                    "self-contained; releases are gitignored automatically",
                ));
                ("releases", lines)
            }
            Step::Summary => {
                let mut lines = text(".tinycd/config.toml will contain:");
                lines.push(Line::default());
                for line in config_toml(&self.answers()).lines() {
                    lines.push(Line::styled(
                        format!("  {line}"),
                        Style::default().fg(Color::Cyan),
                    ));
                }
                lines.push(Line::default());
                lines.extend(text(
                    "Enter saves the file and starts tinycd. Commit .tinycd/config.toml \
                     to share the deployment recipe.",
                ));
                ("summary", lines)
            }
        }
    }
}
