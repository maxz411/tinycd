//! The live dashboard shown while the daemon runs on an interactive
//! terminal: repository and daemon state, retained releases, and the
//! activity feed that plain mode would print. --no-tui disables it.

use std::{
    collections::VecDeque,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Paragraph, Wrap},
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::{App, Error, format_age};

/// How many activity lines are retained for display.
const SCROLLBACK: usize = 300;

/// Run the dashboard until the user quits with q or Ctrl+C. The terminal is
/// restored before returning; a return means "shut the daemon down".
pub async fn dashboard(app: &App, mut lines: UnboundedReceiver<String>) -> Result<(), Error> {
    let (key_tx, mut keys) = unbounded_channel();
    std::thread::spawn(move || {
        while !key_tx.is_closed() {
            if event::poll(Duration::from_millis(200)).unwrap_or(false)
                && let Ok(event) = event::read()
                && key_tx.send(event).is_err()
            {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    let mut activity: VecDeque<String> = VecDeque::new();
    let mut snapshot = Snapshot::read(app).await;
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    let result = loop {
        while let Ok(line) = lines.try_recv() {
            push(&mut activity, line);
        }
        if let Err(error) = terminal.draw(|frame| draw(frame, app, &snapshot, &activity)) {
            break Err(error.into());
        }

        tokio::select! {
            _ = tick.tick() => snapshot = Snapshot::read(app).await,
            Some(line) = lines.recv() => push(&mut activity, line),
            Some(event) = keys.recv() => {
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                    && (key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                {
                    break Ok(());
                }
            }
        }
    };
    ratatui::restore();
    result
}

fn push(activity: &mut VecDeque<String>, line: String) {
    activity.push_back(line);
    while activity.len() > SCROLLBACK {
        activity.pop_front();
    }
}

/// The deployed state as recorded on disk plus whether a server is running.
struct Snapshot {
    current: Option<String>,
    head: Option<String>,
    server_up: bool,
    /// Newest first: (id, incomplete).
    releases: Vec<(String, bool)>,
}

impl Snapshot {
    async fn read(app: &App) -> Self {
        let root = &app.config.root;
        let current = tokio::fs::canonicalize(root.join("current"))
            .await
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
        let head = tokio::fs::read_to_string(root.join("head"))
            .await
            .ok()
            .map(|head| head.trim().to_owned())
            .filter(|head| !head.is_empty());
        let server_up = app.running.lock().await.server.is_some();

        let mut names = Vec::new();
        let mut incomplete = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(root.join("deployments")).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_prefix(".incomplete-") {
                    incomplete.push(id.to_owned());
                } else if !name.starts_with('.')
                    && entry.file_type().await.is_ok_and(|kind| kind.is_dir())
                {
                    names.push(name);
                }
            }
        }
        names.sort();
        names.reverse();
        let releases = names
            .into_iter()
            .map(|name| {
                let building = incomplete.contains(&name);
                (name, building)
            })
            .collect();

        Self {
            current,
            head,
            server_up,
            releases,
        }
    }
}

fn draw(frame: &mut Frame, app: &App, snapshot: &Snapshot, activity: &VecDeque<String>) {
    let releases_height = (snapshot.releases.len().clamp(1, 6) + 2) as u16;
    let [header_area, releases_area, activity_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(releases_height),
        Constraint::Min(3),
    ])
    .areas(frame.area());

    let config = &app.config;
    let mut tracking = config.repo.clone();
    if let Some(git_ref) = &config.git_ref {
        tracking.push_str(&format!(" @ {git_ref}"));
    }
    let mut state = Vec::new();
    if let Some(seconds) = config.poll {
        state.push(format!("polling every {seconds}s"));
    }
    if let Some(address) = config.hook {
        state.push(format!("hook on {address}"));
    }
    state.push(if snapshot.server_up {
        "server up".to_owned()
    } else {
        "server down".to_owned()
    });
    let header = Paragraph::new(vec![
        Line::from(tracking),
        Line::from(format!("root {}", config.root.display())),
        Line::styled(state.join(" · "), Style::default().fg(Color::Cyan)),
    ])
    .block(Block::bordered().title(" tinycd "));
    frame.render_widget(header, header_area);

    let mut rows = Vec::new();
    for (id, building) in snapshot.releases.iter().take(6) {
        let age = id
            .parse::<u64>()
            .ok()
            .and_then(|millis| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH + Duration::from_millis(millis))
                    .ok()
            })
            .map(|age| format!("{} ago", format_age(age)))
            .unwrap_or_default();
        let is_current = snapshot.current.as_deref() == Some(id.as_str());
        let marker = if is_current { "●" } else { " " };
        let mut label = format!(" {marker} {id}  {age}");
        if is_current {
            label.push_str("  current");
            if let Some(head) = &snapshot.head {
                label.push_str(&format!("  {}", &head[..head.len().min(12)]));
            }
        }
        if *building {
            label.push_str("  building");
        }
        let style = if is_current {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        rows.push(Line::styled(label, style));
    }
    if rows.is_empty() {
        rows.push(Line::from(" none yet"));
    }
    let releases = Paragraph::new(rows).block(Block::bordered().title(" releases "));
    frame.render_widget(releases, releases_area);

    let visible = activity_area.height.saturating_sub(2) as usize;
    let tail = activity
        .iter()
        .skip(activity.len().saturating_sub(visible))
        .map(|line| {
            if line.contains("failed") || line.starts_with("error") {
                Line::styled(line.clone(), Style::default().fg(Color::Red))
            } else {
                Line::from(line.clone())
            }
        })
        .collect::<Vec<_>>();
    let feed = Paragraph::new(tail).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .title(" activity ")
            .title_bottom(Line::from(" q quit ").centered()),
    );
    frame.render_widget(feed, activity_area);
}
