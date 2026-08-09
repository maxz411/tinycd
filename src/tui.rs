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
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::{App, Error, format_age};

/// How many activity lines are retained for display.
const SCROLLBACK: usize = 300;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Redraws per snapshot refresh; the redraw tick drives the animations.
const TICKS_PER_SNAPSHOT: u64 = 5;

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
    let truecolor = truecolor();
    let mut activity: VecDeque<String> = VecDeque::new();
    let mut snapshot = Snapshot::read(app).await;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut frame_count: u64 = 0;

    let result = loop {
        while let Ok(line) = lines.try_recv() {
            push(&mut activity, line);
        }
        if let Err(error) =
            terminal.draw(|frame| draw(frame, app, &snapshot, &activity, frame_count, truecolor))
        {
            break Err(error.into());
        }

        tokio::select! {
            _ = tick.tick() => {
                frame_count += 1;
                if frame_count.is_multiple_of(TICKS_PER_SNAPSHOT) {
                    snapshot = Snapshot::read(app).await;
                }
            }
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

/// Whether the terminal advertises 24-bit color. Terminals that support it
/// set COLORTERM=truecolor (or 24bit), or use a -direct TERM entry; anything
/// else gets the 16-color palette so the RGB animations degrade gracefully.
fn truecolor() -> bool {
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    colorterm.contains("truecolor")
        || colorterm.contains("24bit")
        || std::env::var("TERM").is_ok_and(|term| term.contains("direct"))
}

/// A slow triangle wave in 0..=1 used to pulse colors.
fn pulse(frame: u64, period: u64) -> f32 {
    let phase = (frame % period) as f32 / period as f32;
    1.0 - (2.0 * phase - 1.0).abs()
}

/// Pulse a color between a dim floor and full brightness. Without truecolor,
/// blink between the normal and bright variants of the base color instead.
fn glow(rgb: (u8, u8, u8), base: Color, bright: Color, level: f32, truecolor: bool) -> Color {
    let level = level.clamp(0.0, 1.0);
    if !truecolor {
        return if level > 0.5 { bright } else { base };
    }
    let (r, g, b) = rgb;
    let scale = 0.55 + 0.45 * level;
    Color::Rgb(
        (r as f32 * scale) as u8,
        (g as f32 * scale) as u8,
        (b as f32 * scale) as u8,
    )
}

/// A cyan-to-violet gradient that drifts across the title over time. Without
/// truecolor, a coarser wave of the nearest palette colors drifts instead.
fn shimmer(index: usize, frame: u64, truecolor: bool) -> Color {
    let t = index as f32 * 0.45 - frame as f32 * 0.12;
    let x = (t.sin() + 1.0) / 2.0;
    if !truecolor {
        return match (x * 4.0) as u8 {
            0 => Color::Cyan,
            1 => Color::LightCyan,
            2 => Color::LightBlue,
            _ => Color::LightMagenta,
        };
    }
    Color::Rgb((110.0 + x * 120.0) as u8, (220.0 - x * 130.0) as u8, 255)
}

/// A full-width `── title ────` divider replacing the old box borders.
fn rule(title: &str, width: u16, accent: Color) -> Line<'static> {
    let dash = Style::default().fg(Color::DarkGray);
    let fill = "─".repeat((width as usize).saturating_sub(title.chars().count() + 6));
    Line::from(vec![
        Span::styled(" ── ", dash),
        Span::styled(
            title.to_owned(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", dash),
        Span::styled(fill, dash),
    ])
}

fn draw(
    frame: &mut Frame,
    app: &App,
    snapshot: &Snapshot,
    activity: &VecDeque<String>,
    tick: u64,
    truecolor: bool,
) {
    let width = frame.area().width;
    let releases_height = snapshot.releases.len().clamp(1, 6) as u16;
    let [
        header_area,
        releases_rule_area,
        releases_area,
        activity_rule_area,
        activity_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(releases_height),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let config = &app.config;
    let spinner = SPINNER[(tick as usize) % SPINNER.len()];
    let mut title = vec![Span::styled(
        format!(" {spinner} "),
        Style::default().fg(Color::Magenta),
    )];
    for (index, letter) in "tinycd".chars().enumerate() {
        title.push(Span::styled(
            letter.to_string(),
            Style::default()
                .fg(shimmer(index, tick, truecolor))
                .add_modifier(Modifier::BOLD),
        ));
    }
    title.push(Span::styled(
        format!("  {}", config.repo),
        Style::default().fg(Color::White),
    ));
    if let Some(git_ref) = &config.git_ref {
        title.push(Span::styled(" @ ", Style::default().fg(Color::DarkGray)));
        title.push(Span::styled(
            git_ref.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }

    let dot_sep = Span::styled("  ·  ", Style::default().fg(Color::DarkGray));
    let mut state = vec![Span::raw("   ")];
    if snapshot.server_up {
        state.push(Span::styled(
            "●",
            Style::default().fg(glow(
                (80, 220, 130),
                Color::Green,
                Color::LightGreen,
                pulse(tick, 20),
                truecolor,
            )),
        ));
        state.push(Span::styled(
            " server up",
            Style::default().fg(Color::Green),
        ));
    } else {
        state.push(Span::styled("●", Style::default().fg(Color::Red)));
        state.push(Span::styled(
            " server down",
            Style::default().fg(Color::Red),
        ));
    }
    if let Some(seconds) = config.poll {
        state.push(dot_sep.clone());
        state.push(Span::styled(
            format!("polling every {seconds}s"),
            Style::default().fg(Color::Cyan),
        ));
    }
    if let Some(address) = config.hook {
        state.push(dot_sep.clone());
        state.push(Span::styled(
            format!("hook on {address}"),
            Style::default().fg(Color::Cyan),
        ));
    }
    state.push(dot_sep);
    state.push(Span::styled(
        format!("root {}", config.root.display()),
        Style::default().fg(Color::DarkGray),
    ));

    let header = Paragraph::new(vec![Line::default(), Line::from(title), Line::from(state)]);
    frame.render_widget(header, header_area);
    frame.render_widget(
        Paragraph::new(rule("releases", width, Color::Magenta)),
        releases_rule_area,
    );

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
        let mut row = Vec::new();
        if is_current {
            row.push(Span::styled(
                " ● ",
                Style::default().fg(glow(
                    (90, 200, 255),
                    Color::Cyan,
                    Color::LightCyan,
                    pulse(tick, 24),
                    truecolor,
                )),
            ));
            row.push(Span::styled(
                id.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            row.push(Span::styled(
                format!("  {age}"),
                Style::default().fg(Color::DarkGray),
            ));
            row.push(Span::styled("  current", Style::default().fg(Color::Green)));
            if let Some(head) = &snapshot.head {
                row.push(Span::styled(
                    format!("  {}", &head[..head.len().min(12)]),
                    Style::default().fg(Color::Yellow),
                ));
            }
        } else {
            row.push(Span::raw("   "));
            row.push(Span::styled(id.clone(), Style::default().fg(Color::Gray)));
            row.push(Span::styled(
                format!("  {age}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if *building {
            row.push(Span::styled(
                format!("  {spinner} building"),
                Style::default().fg(Color::Yellow),
            ));
        }
        rows.push(Line::from(row));
    }
    if rows.is_empty() {
        rows.push(Line::styled(
            "   none yet",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(rows), releases_area);

    frame.render_widget(
        Paragraph::new(rule("activity", width, Color::Magenta)),
        activity_rule_area,
    );
    let visible = activity_area.height as usize;
    let shown = activity.len().min(visible);
    let tail = activity
        .iter()
        .skip(activity.len() - shown)
        .enumerate()
        .map(|(index, line)| {
            let mut style = if line.contains("failed") || line.starts_with("error") {
                Style::default().fg(Color::Red)
            } else if line.starts_with("deployed") {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with("running ") {
                Style::default().fg(Color::Yellow)
            } else if line.starts_with("tracking")
                || line.starts_with("listening")
                || line.starts_with("deployments in")
            {
                Style::default().fg(Color::Cyan)
            } else if line.starts_with("shutting down") {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default().fg(Color::Gray)
            };
            // Older lines fade so the newest activity draws the eye.
            if index + 1 < shown {
                style = style.add_modifier(Modifier::DIM);
            }
            Line::styled(format!(" {line}"), style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(tail).wrap(Wrap { trim: false }),
        activity_area,
    );

    frame.render_widget(
        Paragraph::new(Line::styled(
            " q quit",
            Style::default().fg(Color::DarkGray),
        )),
        footer_area,
    );
}
