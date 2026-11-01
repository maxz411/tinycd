//! The live terminal dashboard: a client of the hub's API, so it attaches
//! to a hub in this process, in a service, or in a container alike.
//! Projects, hosts, the releases of one project, and the activity feed.

use std::{collections::VecDeque, time::Duration};

use ratatui::{
    Frame,
    crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use tokio::sync::mpsc::unbounded_channel;

use crate::{
    client::Client,
    model::{Event, Overview, Status},
    util::{Error, age_of},
};

/// How many activity lines are retained for display.
const SCROLLBACK: usize = 300;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Run the dashboard until the user quits with q or Ctrl+C. With a focus
/// project, its releases are shown instead of the project list. The
/// terminal is restored before returning.
pub async fn dashboard(client: Client, focus: Option<String>, footer: &str) -> Result<(), Error> {
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

    let mut events = client.events(focus.clone());
    let mut terminal = ratatui::init();
    let truecolor = truecolor();
    let mut activity: VecDeque<String> = VecDeque::new();
    let mut overview = client.overview().await.ok();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut frame_count: u64 = 0;
    let mut dirty = false;
    let mut lost = false;

    let result = loop {
        if let Err(error) = terminal.draw(|frame| {
            draw(
                frame,
                overview.as_ref(),
                focus.as_deref(),
                &activity,
                frame_count,
                truecolor,
                footer,
                lost,
            )
        }) {
            break Err(error.into());
        }

        tokio::select! {
            _ = tick.tick() => {
                frame_count += 1;
                if dirty || frame_count.is_multiple_of(30) {
                    dirty = false;
                    match client.overview().await {
                        Ok(fresh) => { overview = Some(fresh); lost = false; }
                        Err(_) => lost = true,
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Some(Event::Message { text }) => push(&mut activity, text),
                    Some(Event::Deployment { deployment }) => {
                        if !deployment.status.in_progress() || deployment.status == Status::Syncing {
                            push(&mut activity, format!("{}: {} {}", deployment.project, deployment.id, deployment.status.label()));
                        }
                        dirty = true;
                    }
                    Some(Event::Host { host, online }) => {
                        push(&mut activity, format!("host {host} {}", if online { "online" } else { "offline" }));
                        dirty = true;
                    }
                    Some(Event::Log { .. }) => {}
                    Some(_) => dirty = true,
                    None => { lost = true; }
                }
            }
            Some(event) = keys.recv() => {
                if let TermEvent::Key(key) = event
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

/// A full-width `── title ────` divider.
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

fn status_style(status: Status) -> Style {
    match status {
        Status::Live => Style::default().fg(Color::Green),
        Status::Failed | Status::Cancelled => Style::default().fg(Color::Red),
        Status::Retired => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::Yellow),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut Frame,
    overview: Option<&Overview>,
    focus: Option<&str>,
    activity: &VecDeque<String>,
    tick: u64,
    truecolor: bool,
    footer: &str,
    lost: bool,
) {
    let width = frame.area().width;
    let spinner = SPINNER[(tick as usize) % SPINNER.len()];
    let focused = focus.and_then(|id| overview?.projects.iter().find(|p| p.project.id == id));

    let list_len = match (focused, overview) {
        (Some(view), _) => view.deployments.len(),
        (None, Some(overview)) => overview.projects.len(),
        (None, None) => 0,
    };
    let list_height = list_len.clamp(1, 8) as u16;
    let [
        header_area,
        list_rule_area,
        list_area,
        activity_rule_area,
        activity_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(list_height),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

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
    match focused {
        Some(view) => {
            title.push(Span::styled(
                format!("  {}", view.project.repo),
                Style::default().fg(Color::White),
            ));
            if let Some(git_ref) = &view.project.git_ref {
                title.push(Span::styled(" @ ", Style::default().fg(Color::DarkGray)));
                title.push(Span::styled(
                    git_ref.clone(),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        None => {
            if let Some(overview) = overview {
                title.push(Span::styled(
                    format!("  {}", overview.url),
                    Style::default().fg(Color::White),
                ));
            }
        }
    }

    let dot_sep = Span::styled("  ·  ", Style::default().fg(Color::DarkGray));
    let mut state = vec![Span::raw("   ")];
    if lost {
        state.push(Span::styled("●", Style::default().fg(Color::Red)));
        state.push(Span::styled(
            " hub unreachable",
            Style::default().fg(Color::Red),
        ));
    } else if let Some(view) = focused {
        let up = view.project.current.is_some() && view.project.running;
        if up {
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
        if let Some(seconds) = view.project.poll {
            state.push(dot_sep.clone());
            state.push(Span::styled(
                format!("polling every {seconds}s"),
                Style::default().fg(Color::Cyan),
            ));
        }
        if view.project.webhook_token.is_some() {
            state.push(dot_sep.clone());
            state.push(Span::styled("webhook on", Style::default().fg(Color::Cyan)));
        }
        state.push(dot_sep.clone());
        state.push(Span::styled(
            format!("{} on {}", view.project.runtime.label(), view.project.host),
            Style::default().fg(Color::DarkGray),
        ));
        if let Some(root) = &view.project.root {
            state.push(dot_sep);
            state.push(Span::styled(
                format!("root {}", root.display()),
                Style::default().fg(Color::DarkGray),
            ));
        }
    } else if let Some(overview) = overview {
        let online = overview.hosts.iter().filter(|h| h.online).count();
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
            format!(" {} of {} hosts online", online, overview.hosts.len()),
            Style::default().fg(Color::Green),
        ));
        state.push(dot_sep.clone());
        state.push(Span::styled(
            format!("{} projects", overview.projects.len()),
            Style::default().fg(Color::Cyan),
        ));
        state.push(dot_sep);
        state.push(Span::styled(
            format!("up {}", age_of(overview.started_at)),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let header = Paragraph::new(vec![Line::default(), Line::from(title), Line::from(state)]);
    frame.render_widget(header, header_area);

    let mut rows = Vec::new();
    match focused {
        Some(view) => {
            frame.render_widget(
                Paragraph::new(rule("releases", width, Color::Magenta)),
                list_rule_area,
            );
            for d in view.deployments.iter().take(8) {
                let is_current = view.project.current.as_deref() == Some(d.id.as_str());
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
                        d.id.clone(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    row.push(Span::raw("   "));
                    row.push(Span::styled(d.id.clone(), Style::default().fg(Color::Gray)));
                }
                row.push(Span::styled(
                    format!("  {} ago", age_of(d.updated_at)),
                    Style::default().fg(Color::DarkGray),
                ));
                if is_current {
                    row.push(Span::styled("  current", Style::default().fg(Color::Green)));
                }
                if d.status.in_progress() {
                    row.push(Span::styled(
                        format!("  {spinner} {}", d.status.label()),
                        status_style(d.status),
                    ));
                } else if d.status != Status::Live {
                    row.push(Span::styled(
                        format!("  {}", d.status.label()),
                        status_style(d.status),
                    ));
                }
                if !d.commit.is_empty() {
                    row.push(Span::styled(
                        format!("  {}", &d.commit[..d.commit.len().min(12)]),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                rows.push(Line::from(row));
            }
        }
        None => {
            frame.render_widget(
                Paragraph::new(rule("projects", width, Color::Magenta)),
                list_rule_area,
            );
            if let Some(overview) = overview {
                for view in overview.projects.iter().take(8) {
                    let latest = view.deployments.first();
                    let up = view.project.current.is_some() && view.project.running;
                    let mut row = vec![
                        Span::styled(
                            if up { " ● " } else { " ○ " },
                            Style::default().fg(if up { Color::Green } else { Color::Red }),
                        ),
                        Span::styled(
                            view.project.name.clone(),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(d) = latest {
                        let label = if d.status.in_progress() {
                            format!("  {spinner} {}", d.status.label())
                        } else {
                            format!("  {}", d.status.label())
                        };
                        row.push(Span::styled(label, status_style(d.status)));
                        row.push(Span::styled(
                            format!("  {} ago", age_of(d.updated_at)),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    row.push(Span::styled(
                        format!("  {} · {}", view.project.host, view.project.runtime.label()),
                        Style::default().fg(Color::DarkGray),
                    ));
                    row.push(Span::styled(
                        format!("  {}", view.project.repo),
                        Style::default().fg(Color::Gray),
                    ));
                    rows.push(Line::from(row));
                }
            }
        }
    }
    if rows.is_empty() {
        rows.push(Line::styled(
            "   none yet",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(rows), list_area);

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
            } else if line.starts_with("deployed") || line.ends_with(" live") {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with("running ") || line.contains(": deploying") {
                Style::default().fg(Color::Yellow)
            } else if line.starts_with("dashboard at")
                || line.starts_with("project ")
                || line.starts_with("host ")
                || line.starts_with("secure listener")
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
            format!(" {footer}"),
            Style::default().fg(Color::DarkGray),
        )),
        footer_area,
    );
}
