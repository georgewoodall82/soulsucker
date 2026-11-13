//! Ratatui rendering for every screen.

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Gauge, List, ListItem, Padding, Paragraph, Wrap,
};

use crate::app::{
    App, ConnState, DownloadFocus, DownloadPhase, SearchFocus, SearchPhase, SettingKind,
};
use crate::input::TextInput;
use crate::session::{LogLevel, TrackStatus};
use crate::util;

const BG: Color = Color::Rgb(16, 17, 24);
const PANEL: Color = Color::Rgb(23, 24, 34);
const PANEL_ALT: Color = Color::Rgb(31, 32, 45);
const BORDER: Color = Color::Rgb(63, 65, 86);
const TEXT: Color = Color::Rgb(222, 223, 235);
const MUTED: Color = Color::Rgb(137, 140, 160);
const ACCENT: Color = Color::Rgb(190, 137, 255);
const ACCENT_DARK: Color = Color::Rgb(96, 61, 145);
const SUCCESS: Color = Color::Rgb(112, 220, 162);
const WARNING: Color = Color::Rgb(240, 190, 100);
const ERROR: Color = Color::Rgb(240, 112, 128);
const INFO: Color = Color::Rgb(105, 190, 240);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    frame.render_widget(Block::default().style(base_style()), area);
    if area.width < 48 || area.height < 10 {
        let message = Paragraph::new(vec![
            Line::from(Span::styled(
                "soulsucker",
                Style::default().fg(ACCENT).bold(),
            )),
            Line::from(Span::styled(
                "Please make the terminal at least 48×10.",
                Style::default().fg(MUTED),
            )),
        ])
        .alignment(Alignment::Center);
        frame.render_widget(message, area);
        return;
    }

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    render_header(app, frame, header);
    match app.screen {
        crate::app::Screen::Setup => render_setup(app, frame, body),
        crate::app::Screen::Search => render_search(app, frame, body),
        crate::app::Screen::Tracks => render_tracks(app, frame, body),
        crate::app::Screen::Download => render_download(app, frame, body),
        crate::app::Screen::Settings => render_settings(app, frame, body),
    }
    render_footer(app, frame, footer);

    if app.screen == crate::app::Screen::Settings && app.settings.editing.is_some() {
        render_edit_popup(app, frame, area);
    }
    if let Some(confirm) = &app.confirm {
        render_confirm(confirm, frame, area);
    }
    if app.help_open {
        render_help(app, frame, area);
    }
}

fn base_style() -> Style {
    Style::default().bg(BG).fg(TEXT)
}

fn spinner(app: &App) -> &'static str {
    SPINNER[app.spinner % SPINNER.len()]
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { BORDER }))
        .style(Style::default().bg(PANEL).fg(TEXT))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title.to_string(),
                Style::default()
                    .fg(if focused { ACCENT } else { MUTED })
                    .bold(),
            ),
            Span::raw(" "),
        ]))
}

fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    let (dot, label, color) = match &app.connection {
        ConnState::Disconnected => ("○", "offline".to_string(), MUTED),
        ConnState::Connecting => (spinner(app), "connecting to Soulseek…".to_string(), WARNING),
        ConnState::Online { username, port } => (
            "●",
            match port {
                Some(port) => format!("{username} · port {port}"),
                None => username.clone(),
            },
            SUCCESS,
        ),
        ConnState::Error(message) => (
            "●",
            format!("Soulseek offline · {}", util::truncate(message, 28)),
            ERROR,
        ),
    };
    let label = util::truncate(&label, 34);
    let right_width = (util::display_width(dot) + 1 + util::display_width(&label)) as u16 + 1;
    let [left, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right_width.min(area.width)),
    ])
    .areas(area);

    let left_line = Line::from(vec![
        Span::styled(" ♫ ", Style::default().fg(ACCENT).bold()),
        Span::styled("soulsucker", Style::default().fg(TEXT).bold()),
        Span::styled("  MusicBrainz → Soulseek", Style::default().fg(MUTED)),
    ]);
    frame.render_widget(Paragraph::new(left_line), left);
    let right_line = Line::from(vec![
        Span::styled(format!("{dot} "), Style::default().fg(color).bold()),
        Span::styled(label, Style::default().fg(color)),
    ]);
    frame.render_widget(
        Paragraph::new(right_line).alignment(Alignment::Right),
        right,
    );
}

fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(PANEL_ALT)), area);
    let hints = footer_hints(app);
    let hints: &[(&'static str, &'static str)] = if app.toast.is_some() {
        &hints[..hints.len().min(4)]
    } else {
        &hints[..]
    };
    let mut spans = Vec::new();
    for (index, (key, action)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::default().fg(BORDER)));
        }
        spans.push(Span::styled(
            format!("[{key}]"),
            Style::default().fg(ACCENT).bold(),
        ));
        spans.push(Span::styled(
            format!(" {action}"),
            Style::default().fg(MUTED),
        ));
    }

    let Some(toast) = &app.toast else {
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(PANEL_ALT)),
            area,
        );
        return;
    };

    let (icon, color) = toast_style(toast.level);
    let text = util::truncate(
        &format!("{icon} {}", toast.message),
        (area.width as usize / 2).max(16),
    );
    let width = util::display_width(&text) as u16 + 2;
    let [left, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(width.min(area.width)),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(PANEL_ALT)),
        left,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {text} "),
            Style::default().fg(color).bold(),
        )))
        .alignment(Alignment::Right)
        .style(Style::default().bg(PANEL_ALT)),
        right,
    );
}

fn footer_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    if app.help_open {
        return vec![("Esc", "close help")];
    }
    if app.confirm.is_some() {
        return vec![("y", "yes"), ("n", "no")];
    }
    if app.screen == crate::app::Screen::Settings {
        return vec![
            ("↑↓", "select"),
            ("Enter", "edit/toggle"),
            ("Ctrl+S", "save & back"),
            ("Esc", "discard & back"),
        ];
    }
    match app.screen {
        crate::app::Screen::Setup => vec![
            ("Tab", "next field"),
            ("Enter", "continue"),
            ("Ctrl+S", "save"),
            ("F1", "help"),
            ("Esc", "quit"),
        ],
        crate::app::Screen::Search => {
            if app.search.phase.is_loading() {
                vec![("Esc", "cancel"), ("F2", "settings"), ("Ctrl+Q", "quit")]
            } else if app.search.focus == SearchFocus::Results {
                vec![
                    ("↑↓", "select"),
                    ("Enter", "open release"),
                    ("/", "edit"),
                    ("r", "search again"),
                    ("s", "settings"),
                    ("q", "quit"),
                ]
            } else {
                vec![
                    ("Tab", "next field"),
                    ("Enter", "search"),
                    ("Esc", "results"),
                    ("F2", "settings"),
                    ("Ctrl+Q", "quit"),
                ]
            }
        }
        crate::app::Screen::Tracks => vec![
            ("↑↓", "scroll"),
            ("d", "download"),
            ("f", "fingerprints"),
            ("b", "back"),
            ("s", "settings"),
            ("q", "quit"),
        ],
        crate::app::Screen::Download => match app.download.as_ref().map(|view| view.focus) {
            Some(DownloadFocus::Log) => vec![
                ("↑↓", "scroll"),
                ("Tab", "tracks"),
                ("r", "retry failed"),
                ("n", "new search"),
                ("q", "quit"),
            ],
            _ => {
                let complete = app
                    .download
                    .as_ref()
                    .is_some_and(|view| view.phase == DownloadPhase::Complete);
                if complete {
                    vec![
                        ("↑↓", "select"),
                        ("Tab", "activity"),
                        ("r", "retry failed"),
                        ("n", "new search"),
                        ("o", "folder"),
                        ("Esc", "back"),
                        ("q", "quit"),
                    ]
                } else {
                    vec![
                        ("↑↓", "select"),
                        ("Tab", "activity"),
                        ("x", "cancel track"),
                        ("r", "retry failed"),
                        ("n", "new search"),
                        ("o", "folder"),
                        ("q", "quit"),
                    ]
                }
            }
        },
        crate::app::Screen::Settings => Vec::new(),
    }
}

fn toast_style(level: LogLevel) -> (&'static str, Color) {
    match level {
        LogLevel::Info => ("·", INFO),
        LogLevel::Success => ("✔", SUCCESS),
        LogLevel::Warn => ("!", WARNING),
        LogLevel::Error => ("✘", ERROR),
    }
}

// -------------------------------------------------------------------------
// Setup
// -------------------------------------------------------------------------

fn render_setup(app: &mut App, frame: &mut Frame, area: Rect) {
    let form_width = area.width.saturating_sub(4).clamp(44, 82);
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(15),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, form_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(form_width),
        Constraint::Fill(1),
    ])
    .areas(middle);

    let block = panel("Welcome to soulsucker", true);
    let inner = block.inner(form_area);
    frame.render_widget(block, form_area);
    if inner.height < 8 {
        return;
    }

    let mut y = inner.y;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "One-time setup. Your login and optional AcoustID key are saved to:",
            Style::default().fg(MUTED),
        ))),
        Rect::new(inner.x, y, inner.width, 1),
    );
    y += 1;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            util::truncate(&app.config_path_label, inner.width as usize),
            Style::default().fg(ACCENT),
        ))),
        Rect::new(inner.x, y, inner.width, 1),
    );
    y += 2;

    let allow_cursor = !app.help_open;
    render_input_row(
        frame,
        inner,
        y,
        "Soulseek user",
        &app.setup.username,
        app.setup.focus == 0,
        false,
        "your Soulseek username",
        allow_cursor,
    );
    y += 1;
    render_input_row(
        frame,
        inner,
        y,
        "Soulseek pass",
        &app.setup.password,
        app.setup.focus == 1,
        true,
        "your password",
        allow_cursor,
    );
    y += 1;
    render_input_row(
        frame,
        inner,
        y,
        "AcoustID key",
        &app.setup.acoustid,
        app.setup.focus == 2,
        true,
        "optional (fingerprint checks)",
        allow_cursor,
    );
    y += 2;

    let notes = [
        ("AcoustID is free: https://acoustid.org/api", MUTED),
        ("Without a key, files are still checked by duration.", MUTED),
        ("Everything can be changed later in Settings (F2).", MUTED),
    ];
    for (note, color) in notes {
        if y >= inner.bottom() {
            break;
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                util::truncate(note, inner.width as usize),
                Style::default().fg(color),
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }

    if let Some(error) = &app.setup.error
        && y < inner.bottom()
    {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                util::truncate(error, inner.width as usize),
                Style::default().fg(ERROR).bold(),
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

// -------------------------------------------------------------------------
// Search
// -------------------------------------------------------------------------

fn render_search(app: &mut App, frame: &mut Frame, area: Rect) {
    let [form_area, results_area, details_area] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(6),
        Constraint::Length(4),
    ])
    .areas(area);

    let form_title = match app.search.phase {
        SearchPhase::Idle => "Search MusicBrainz".to_string(),
        SearchPhase::Searching { .. } => {
            format!("Search MusicBrainz · {} searching…", spinner(app))
        }
        SearchPhase::Fetching { .. } => {
            format!("Search MusicBrainz · {} loading tracklist…", spinner(app))
        }
    };
    let form_block = panel(&form_title, app.search.focus != SearchFocus::Results);
    let form_inner = form_block.inner(form_area);
    frame.render_widget(form_block, form_area);
    let allow_cursor = !app.help_open
        && !app.search.phase.is_loading()
        && app.search.focus != SearchFocus::Results;
    let mut y = form_inner.y;
    render_input_row(
        frame,
        form_inner,
        y,
        "Artist",
        &app.search.artist,
        app.search.focus == SearchFocus::Artist,
        false,
        "artist name",
        allow_cursor,
    );
    y += 1;
    render_input_row(
        frame,
        form_inner,
        y,
        "Album",
        &app.search.album,
        app.search.focus == SearchFocus::Album,
        false,
        "album title",
        allow_cursor,
    );
    y += 1;
    if y < form_inner.bottom() {
        let hint = if app.search.phase.is_loading() {
            format!(
                "{} searching…  {}",
                spinner(app),
                elapsed_label(app.search.phase.started())
            )
        } else if app.search.focus == SearchFocus::Results {
            "Enter opens the highlighted release · / edits the album".to_string()
        } else if app.search.focus == SearchFocus::Artist {
            "Enter moves to the album · Tab switches fields".to_string()
        } else {
            "Enter searches · Tab switches fields".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                util::truncate(&hint, form_inner.width as usize),
                Style::default().fg(MUTED),
            ))),
            Rect::new(form_inner.x, y, form_inner.width, 1),
        );
    }

    let results_title = if app.search.results.is_empty() {
        "Releases".to_string()
    } else {
        format!("Releases · {}", app.search.results.len())
    };
    let results_block = panel(&results_title, app.search.focus == SearchFocus::Results);
    let results_inner = results_block.inner(results_area);
    frame.render_widget(results_block, results_area);

    if let Some(error) = &app.search.error {
        let text = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Could not complete that search",
                Style::default().fg(ERROR).bold(),
            )),
            Line::from(Span::styled(
                util::truncate(error, results_inner.width as usize),
                Style::default().fg(ERROR),
            )),
        ]))
        .wrap(Wrap { trim: false });
        frame.render_widget(text, results_inner);
    } else if matches!(app.search.phase, SearchPhase::Searching { .. }) {
        let text = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                format!("{}  searching MusicBrainz…", spinner(app)),
                Style::default().fg(ACCENT).bold(),
            )),
            Line::from(Span::styled(
                util::truncate(&app.search.query_label, results_inner.width as usize),
                Style::default().fg(MUTED),
            )),
        ]))
        .alignment(Alignment::Center);
        let centered = centered_rect(results_inner, 60, 3);
        frame.render_widget(text, centered);
    } else if app.search.results.is_empty() {
        let text = Paragraph::new(Text::from(vec![
            Line::from(Span::styled("♪", Style::default().fg(ACCENT_DARK))),
            Line::from(Span::styled(
                "Type an artist and album, then press Enter.",
                Style::default().fg(MUTED),
            )),
        ]))
        .alignment(Alignment::Center);
        frame.render_widget(text, results_inner);
    } else {
        let mut items = Vec::with_capacity(app.search.results.len());
        let selected = app.search.selected();
        let width = results_inner.width as usize;
        for (index, release) in app.search.results.iter().enumerate() {
            let is_selected = selected == Some(index);
            let marker = if is_selected { "▌ " } else { "  " };
            let star = if index == 0 { "★ " } else { "  " };
            let mut line1 = vec![
                Span::styled(marker, Style::default().fg(ACCENT).bold()),
                Span::styled(format!("{:>2}. ", index + 1), Style::default().fg(MUTED)),
                Span::styled(
                    star,
                    Style::default().fg(if index == 0 { WARNING } else { MUTED }),
                ),
                Span::styled(
                    util::truncate(&release.title, width.saturating_sub(16)),
                    Style::default().fg(TEXT).bold(),
                ),
                Span::styled("  ·  ", Style::default().fg(BORDER)),
                Span::styled(
                    util::truncate(&release.artist, width.saturating_sub(30).max(8)),
                    Style::default().fg(MUTED),
                ),
            ];
            if index == 0 {
                line1.push(Span::styled("  recommended", Style::default().fg(WARNING)));
            }

            let mut details = String::new();
            if !release.year().is_empty() {
                details.push_str(release.year());
                details.push_str(" · ");
            }
            if !release.country.is_empty() {
                details.push_str(&release.country);
                details.push_str(" · ");
            }
            if !release.status.is_empty() {
                details.push_str(&release.status);
                details.push_str(" · ");
            }
            details.push_str(&release.format_summary());
            details.push_str(&format!(" · {} tracks", release.track_count));
            if !release.disambiguation.is_empty() {
                details.push_str(" · ");
                details.push_str(&release.disambiguation);
            }

            let mut line2 = vec![
                Span::styled("     ", Style::default()),
                Span::styled(
                    util::truncate(&details, width.saturating_sub(8)),
                    Style::default().fg(MUTED),
                ),
            ];
            let tags = release.edition_tags();
            if !tags.is_empty() {
                line2.push(Span::styled("  ", Style::default()));
                line2.push(Span::styled(
                    util::truncate(&tags.join(" · "), width.saturating_sub(20).max(8)),
                    Style::default().fg(WARNING),
                ));
            }
            items.push(ListItem::new(Text::from(vec![
                Line::from(line1),
                Line::from(line2),
            ])));
        }
        let list = List::new(items)
            .block(Block::default().style(Style::default().bg(PANEL)))
            .highlight_style(Style::default().bg(PANEL_ALT));
        frame.render_stateful_widget(list, results_inner, &mut app.search.list_state);

        if matches!(app.search.phase, SearchPhase::Fetching { .. }) {
            let text = Paragraph::new(Line::from(vec![
                Span::styled(spinner(app), Style::default().fg(ACCENT)),
                Span::styled("  fetching tracklist…", Style::default().fg(ACCENT).bold()),
            ]))
            .style(Style::default().bg(PANEL_ALT));
            if results_inner.height > 0 {
                frame.render_widget(
                    text,
                    Rect::new(results_inner.x, results_inner.y, results_inner.width, 1),
                );
            }
        }
    }

    let details_block = panel("Selection", false);
    let details_inner = details_block.inner(details_area);
    frame.render_widget(details_block, details_area);
    if let Some(index) = app.search.selected() {
        if let Some(release) = app.search.results.get(index) {
            let mut line1 = vec![
                Span::styled(
                    if index == 0 { "★ " } else { "  " },
                    Style::default().fg(WARNING),
                ),
                Span::styled(
                    util::truncate(
                        &format!("{} — {}", release.artist, release.title),
                        details_inner.width as usize,
                    ),
                    Style::default().fg(TEXT).bold(),
                ),
            ];
            let tags = release.edition_tags();
            if !tags.is_empty() {
                line1.push(Span::styled("  ", Style::default()));
                line1.push(Span::styled(
                    format!("[{}]", tags.join(" · ")),
                    Style::default().fg(WARNING),
                ));
            }
            let mut line2 = String::new();
            if !release.year().is_empty() {
                line2.push_str(release.year());
                line2.push_str(" · ");
            }
            if !release.country.is_empty() {
                line2.push_str(&release.country);
                line2.push_str(" · ");
            }
            line2.push_str(if release.status.is_empty() {
                "status unknown"
            } else {
                &release.status
            });
            line2.push_str(&format!(
                " · {} · {} tracks",
                release.format_summary(),
                release.track_count
            ));
            if release
                .edition_tags()
                .iter()
                .any(|tag| matches!(tag.as_str(), "Deluxe" | "Live" | "Anniversary" | "Expanded"))
            {
                line2.push_str("  ·  special editions rank below the standard album");
            }
            let text = Text::from(vec![
                Line::from(line1),
                Line::from(Span::styled(
                    util::truncate(&line2, details_inner.width as usize),
                    Style::default().fg(MUTED),
                )),
            ]);
            frame.render_widget(Paragraph::new(text), details_inner);
        }
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Pick a release to see its details.",
                Style::default().fg(MUTED),
            ))),
            details_inner,
        );
    }
}

fn elapsed_label(started: Option<std::time::Instant>) -> String {
    started
        .map(|started| util::fmt_elapsed(started.elapsed()))
        .unwrap_or_else(|| "0:00".to_string())
}

// -------------------------------------------------------------------------
// Tracks
// -------------------------------------------------------------------------

fn render_tracks(app: &mut App, frame: &mut Frame, area: Rect) {
    let (fingerprint_state, fingerprint_ok) = app.fingerprint_state();
    let Some(tracks) = &mut app.tracks else {
        return;
    };
    let [header_area, list_area, options_area] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(6),
        Constraint::Length(4),
    ])
    .areas(area);

    let header_block = panel("Release", false);
    let header_inner = header_block.inner(header_area);
    frame.render_widget(header_block, header_area);
    let release = &tracks.release;
    let mut y = header_inner.y;
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            util::truncate(
                &format!("{} — {}", release.artist, release.title),
                header_inner.width as usize,
            ),
            Style::default().fg(ACCENT).bold(),
        )])),
        Rect::new(header_inner.x, y, header_inner.width, 1),
    );
    y += 1;
    let mut meta = String::new();
    if !release.year().is_empty() {
        meta.push_str(release.year());
        meta.push_str(" · ");
    }
    if !release.country.is_empty() {
        meta.push_str(&release.country);
        meta.push_str(" · ");
    }
    if !release.status.is_empty() {
        meta.push_str(&release.status);
        meta.push_str(" · ");
    }
    if !release.disambiguation.is_empty() {
        meta.push_str(&release.disambiguation);
        meta.push_str(" · ");
    }
    meta.push_str(&format!(
        "{} tracks · {}",
        release.tracks.len(),
        util::fmt_duration(release.total_duration_secs())
    ));
    if release.disc_count() > 1 {
        meta.push_str(&format!(" · {} discs", release.disc_count()));
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            util::truncate(&meta, header_inner.width as usize),
            Style::default().fg(MUTED),
        ))),
        Rect::new(header_inner.x, y, header_inner.width, 1),
    );
    y += 1;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("→ ", Style::default().fg(MUTED)),
            Span::styled(
                util::truncate(
                    &tracks.album_dir.display().to_string(),
                    header_inner.width.saturating_sub(2) as usize,
                ),
                Style::default().fg(TEXT),
            ),
        ])),
        Rect::new(header_inner.x, y, header_inner.width, 1),
    );
    y += 1;
    let mut extra = format!("fingerprint: {fingerprint_state}");
    if tracks.existing_count > 0 {
        extra.push_str(&format!(
            " · {} existing file(s) will be skipped",
            tracks.existing_count
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            util::truncate(&extra, header_inner.width as usize),
            Style::default().fg(if fingerprint_ok { MUTED } else { WARNING }),
        ))),
        Rect::new(header_inner.x, y, header_inner.width, 1),
    );

    let list_block = panel("Tracklist", true);
    let list_inner = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);
    let width = list_inner.width as usize;
    let selected = tracks.selected();
    let mut items = Vec::with_capacity(tracks.tracks.len());
    for (index, planned) in tracks.tracks.iter().enumerate() {
        let is_selected = selected == Some(index);
        let marker = if is_selected { "▌ " } else { "  " };
        let duration = planned
            .meta
            .expected_secs
            .map(util::fmt_duration)
            .unwrap_or_else(|| "--:--".to_string());
        let title_width = width.saturating_sub(17).max(8);
        items.push(ListItem::new(Line::from(vec![
            Span::styled(marker, Style::default().fg(ACCENT).bold()),
            Span::styled(
                format!("{:>5}  ", planned.label),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                util::fit(&planned.meta.title, title_width),
                Style::default().fg(TEXT),
            ),
            Span::styled(format!("{duration:>8}"), Style::default().fg(MUTED)),
        ])));
    }
    let list = List::new(items)
        .block(Block::default().style(Style::default().bg(PANEL)))
        .highlight_style(Style::default().bg(PANEL_ALT));
    frame.render_stateful_widget(list, list_inner, &mut tracks.list_state);

    let options_block = panel("Options", false);
    let options_inner = options_block.inner(options_area);
    frame.render_widget(options_block, options_area);
    let mut line1 = format!(
        "{} parallel · {}s search · {}s queue · {}s stall · ±{}s length · {} source(s)/track",
        app.cfg.max_concurrent,
        app.cfg.search_timeout_secs,
        app.cfg.queue_timeout_secs,
        app.cfg.stall_timeout_secs,
        app.cfg.length_tolerance_secs,
        app.cfg.candidates_per_track
    );
    if app.force {
        line1.push_str(" · force re-download");
    }
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                util::truncate(&line1, options_inner.width as usize),
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                util::truncate(
                    &format!("Music folder: {}", app.cfg.music_dir),
                    options_inner.width as usize,
                ),
                Style::default().fg(MUTED),
            )),
        ])),
        options_inner,
    );
}

// -------------------------------------------------------------------------
// Download
// -------------------------------------------------------------------------

fn render_download(app: &mut App, frame: &mut Frame, area: Rect) {
    let spin = spinner(app);
    let offline = matches!(
        app.connection,
        ConnState::Error(_) | ConnState::Disconnected
    );
    let Some(view) = &mut app.download else {
        return;
    };
    let log_height = if area.height >= 26 { 8 } else { 5 };
    let [summary_area, tracks_area, log_area] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(6),
        Constraint::Length(log_height),
    ])
    .areas(area);

    // Summary
    let summary_block = panel("Download", false);
    let summary_inner = summary_block.inner(summary_area);
    frame.render_widget(summary_block, summary_area);
    if summary_inner.height >= 1 {
        let title = format!("{} — {}", view.artist, view.album);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                util::truncate(&title, summary_inner.width as usize),
                Style::default().fg(ACCENT).bold(),
            )])),
            Rect::new(summary_inner.x, summary_inner.y, summary_inner.width, 1),
        );
    }

    let total = view.rows.len().max(1);
    let progress: f64 = view.rows.iter().map(row_progress).sum::<f64>() / total as f64;
    let terminated = view.terminated();
    let gauge_color = if view.phase == DownloadPhase::Complete {
        if view.failed == 0 && view.cancelled == 0 {
            SUCCESS
        } else {
            WARNING
        }
    } else if view.phase == DownloadPhase::Waiting {
        INFO
    } else {
        ACCENT
    };
    let gauge_label = match view.phase {
        DownloadPhase::Waiting => {
            if offline {
                "Soulseek offline — F2 settings · F5 retry".to_string()
            } else {
                "waiting for Soulseek login…".to_string()
            }
        }
        DownloadPhase::Searching => format!(
            "{} searching Soulseek · {} peer(s) responded",
            spin, view.peers
        ),
        DownloadPhase::Downloading => format!(
            "{terminated}/{} tracks · {:.0}%",
            view.rows.len(),
            progress * 100.0
        ),
        DownloadPhase::Complete => {
            format!("{}/{} tracks · finished", view.completed, view.rows.len())
        }
    };
    if summary_inner.height >= 2 {
        let gauge_area = Rect::new(summary_inner.x, summary_inner.y + 1, summary_inner.width, 1);
        let gauge = Gauge::default()
            .ratio(progress.clamp(0.0, 1.0))
            .gauge_style(
                Style::default()
                    .fg(gauge_color)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            )
            .label(Span::styled(
                util::truncate(&gauge_label, summary_inner.width.saturating_sub(2) as usize),
                Style::default().fg(TEXT).bold(),
            ));
        frame.render_widget(gauge, gauge_area);
    }

    if summary_inner.height >= 3 {
        let elapsed = view
            .finished
            .unwrap_or_else(std::time::Instant::now)
            .duration_since(view.started);
        let bytes: u64 = view.rows.iter().map(|row| row.bytes).sum();
        let speed: f64 = view
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.status,
                    TrackStatus::Downloading | TrackStatus::Verifying
                )
            })
            .map(|row| row.speed)
            .sum();
        let active = view
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.status,
                    TrackStatus::Downloading | TrackStatus::Verifying
                )
            })
            .count();
        let retrying = view
            .rows
            .iter()
            .filter(|row| row.status == TrackStatus::Retrying)
            .count();
        let failed = view
            .rows
            .iter()
            .filter(|row| row.status == TrackStatus::Failed)
            .count();
        let mut stats = match view.finished {
            Some(finished) => format!(
                "finished in {} · {} transferred",
                util::fmt_elapsed(finished.duration_since(view.started)),
                util::fmt_bytes(bytes)
            ),
            None => format!(
                "{} elapsed · {} transferred",
                util::fmt_elapsed(elapsed),
                util::fmt_bytes(bytes)
            ),
        };
        if speed > 0.0 {
            stats.push_str(&format!(" · {}", util::fmt_speed(speed)));
        }
        if active > 0 {
            stats.push_str(&format!(" · {active} active"));
        }
        if retrying > 0 {
            stats.push_str(&format!(" · {retrying} retrying"));
        }
        if failed > 0 {
            stats.push_str(&format!(" · {failed} failed"));
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                util::truncate(&stats, summary_inner.width as usize),
                Style::default().fg(MUTED),
            ))),
            Rect::new(summary_inner.x, summary_inner.y + 2, summary_inner.width, 1),
        );
    }

    // Tracks
    let tracks_block = panel("Tracks", view.focus == DownloadFocus::Tracks);
    let tracks_inner = tracks_block.inner(tracks_area);
    frame.render_widget(tracks_block, tracks_area);
    let width = tracks_inner.width as usize;
    let selected = view.selected();
    let bar_width = if width >= 100 {
        18
    } else if width >= 82 {
        14
    } else {
        10
    };
    let show_meta = width >= 88;
    let show_note = width >= 72;
    let meta_width = if show_meta { 12 } else { 0 };
    let note_width = if show_note {
        (width / 4).clamp(8, 24)
    } else {
        0
    };
    let title_width = width
        .saturating_sub(2 + 2 + 6 + bar_width + 1 + 4 + meta_width + note_width + 2)
        .max(8);
    let mut items = Vec::with_capacity(view.rows.len());
    for (index, row) in view.rows.iter().enumerate() {
        let is_selected = selected == Some(index);
        let marker = if is_selected { "▌ " } else { "  " };
        let (icon, icon_color) = status_icon(row.status, spin);
        let (bar, bar_color) = match row.status {
            TrackStatus::Done => (util::progress_bar(1.0, bar_width), SUCCESS),
            TrackStatus::Failed => (util::progress_bar(row_progress(row), bar_width), ERROR),
            TrackStatus::Cancelled => (util::progress_bar(row_progress(row), bar_width), MUTED),
            TrackStatus::Verifying => (util::progress_bar(row_progress(row), bar_width), INFO),
            TrackStatus::Retrying => (util::progress_bar(row_progress(row), bar_width), WARNING),
            TrackStatus::Downloading => (util::progress_bar(row_progress(row), bar_width), ACCENT),
            TrackStatus::Queued => (util::progress_bar(0.0, bar_width), BORDER),
        };
        let pct = if row.status == TrackStatus::Done {
            100
        } else {
            (row_progress(row) * 100.0).round() as u32
        };
        let meta = if show_meta {
            if matches!(
                row.status,
                TrackStatus::Downloading | TrackStatus::Verifying
            ) {
                util::fit(&util::fmt_speed(row.speed), meta_width)
            } else if row.status == TrackStatus::Done {
                util::fit(&util::fmt_bytes(row.bytes), meta_width)
            } else {
                util::fit("", meta_width)
            }
        } else {
            String::new()
        };
        let note = if show_note {
            let text = if !row.note.is_empty() && !matches!(row.status, TrackStatus::Downloading) {
                row.note.clone()
            } else if !row.source.is_empty() {
                if row.attempts_total > 1 {
                    format!("@{} {}/{}", row.source, row.attempt, row.attempts_total)
                } else {
                    format!("@{}", row.source)
                }
            } else {
                row.note.clone()
            };
            util::fit(&text, note_width)
        } else {
            String::new()
        };
        let note_color = match row.status {
            TrackStatus::Failed => ERROR,
            TrackStatus::Retrying => WARNING,
            TrackStatus::Verifying => INFO,
            TrackStatus::Done => MUTED,
            _ => MUTED,
        };

        let mut spans = vec![
            Span::styled(marker, Style::default().fg(ACCENT).bold()),
            Span::styled(format!("{icon} "), Style::default().fg(icon_color).bold()),
            Span::styled(format!("{:>5} ", row.label), Style::default().fg(MUTED)),
            Span::styled(
                util::fit(&row.title, title_width),
                Style::default().fg(TEXT),
            ),
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::styled(format!(" {pct:>3}% "), Style::default().fg(MUTED)),
        ];
        if show_meta {
            spans.push(Span::styled(meta, Style::default().fg(MUTED)));
        }
        if show_note {
            spans.push(Span::styled(note, Style::default().fg(note_color)));
        }
        items.push(ListItem::new(Line::from(spans)));
    }
    let list = List::new(items)
        .block(Block::default().style(Style::default().bg(PANEL)))
        .highlight_style(Style::default().bg(PANEL_ALT));
    frame.render_stateful_widget(list, tracks_inner, &mut view.list_state);

    // Activity log
    let log_height = log_area.height.saturating_sub(2) as usize;
    let max_offset = app.logs.len().saturating_sub(log_height);
    let log_offset = view.log_offset.min(max_offset);
    let log_title = if log_offset > 0 {
        format!("Activity · ↑ {log_offset} scrolled")
    } else {
        "Activity".to_string()
    };
    let log_block = panel(&log_title, view.focus == DownloadFocus::Log);
    let log_inner = log_block.inner(log_area);
    frame.render_widget(log_block, log_area);
    let height = log_inner.height as usize;
    let end = app.logs.len().saturating_sub(log_offset);
    let start = end.saturating_sub(height);
    let mut lines = Vec::new();
    for log in &app.logs[start..end] {
        let (icon, color) = toast_style(log.level);
        let stamp = util::fmt_elapsed(log.at.duration_since(app.started));
        let text_width = log_inner.width.saturating_sub(12) as usize;
        lines.push(Line::from(vec![
            Span::styled(format!("[{stamp}] "), Style::default().fg(BORDER)),
            Span::styled(format!("{icon} "), Style::default().fg(color).bold()),
            Span::styled(
                util::truncate(&log.message, text_width),
                Style::default().fg(if log.level == LogLevel::Error {
                    ERROR
                } else {
                    TEXT
                }),
            ),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Events will appear here.",
            Style::default().fg(MUTED),
        )));
    }
    frame.render_widget(Paragraph::new(lines), log_inner);
}

fn row_progress(row: &crate::app::DownloadRow) -> f64 {
    match row.status {
        TrackStatus::Done => 1.0,
        TrackStatus::Failed | TrackStatus::Cancelled => 1.0,
        _ if row.total > 0 => (row.bytes as f64 / row.total as f64).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

fn status_icon(status: TrackStatus, spinner: &'static str) -> (&'static str, Color) {
    match status {
        TrackStatus::Queued => ("○", MUTED),
        TrackStatus::Downloading => (spinner, ACCENT),
        TrackStatus::Verifying => ("◌", INFO),
        TrackStatus::Retrying => ("↻", WARNING),
        TrackStatus::Done => ("✔", SUCCESS),
        TrackStatus::Failed => ("✘", ERROR),
        TrackStatus::Cancelled => ("–", MUTED),
    }
}

// -------------------------------------------------------------------------
// Settings
// -------------------------------------------------------------------------

fn render_settings(app: &mut App, frame: &mut Frame, area: Rect) {
    let [hint_area, list_area, error_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Settings file: ", Style::default().fg(MUTED)),
            Span::styled(
                util::truncate(
                    &app.cfg_path.display().to_string(),
                    hint_area.width.saturating_sub(16) as usize,
                ),
                Style::default().fg(ACCENT),
            ),
        ])),
        hint_area,
    );

    let block = panel("Settings", true);
    let inner = block.inner(list_area);
    frame.render_widget(block, list_area);
    let width = inner.width as usize;
    let selected = app.settings.selected;
    let mut items = Vec::with_capacity(app.settings.fields.len());
    for (index, field) in app.settings.fields.iter().enumerate() {
        let is_selected = selected == index;
        let marker = if is_selected { "▌ " } else { "  " };
        let label_width = 22;
        let value_width = if width >= 100 { 30 } else { 20 };
        let value = match field.kind {
            SettingKind::Secret => {
                if field.value.is_empty() {
                    "<not set>".to_string()
                } else {
                    "••••••••".to_string()
                }
            }
            SettingKind::Bool => format!("[{}]", field.value),
            _ => field.value.clone(),
        };
        let value_color = match field.kind {
            SettingKind::Bool if field.value == "on" => SUCCESS,
            SettingKind::Secret if field.value.is_empty() => MUTED,
            _ => TEXT,
        };
        let hint_width = width.saturating_sub(2 + 2 + label_width + value_width + 3);
        items.push(ListItem::new(Line::from(vec![
            Span::styled(marker, Style::default().fg(ACCENT).bold()),
            Span::styled(
                util::fit(field.label, label_width),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                util::fit(&value, value_width),
                Style::default().fg(value_color),
            ),
            Span::styled(
                util::truncate(field.hint, hint_width),
                Style::default().fg(BORDER),
            ),
        ])));
    }
    let list = List::new(items)
        .block(Block::default().style(Style::default().bg(PANEL)))
        .highlight_style(Style::default().bg(PANEL_ALT));
    frame.render_stateful_widget(list, inner, &mut app.settings.list_state);

    if let Some(error) = &app.settings.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                util::truncate(error, error_area.width as usize),
                Style::default().fg(ERROR).bold(),
            ))),
            error_area,
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter edits the selected field; Control+S saves. Esc leaves without saving.",
                Style::default().fg(BORDER),
            ))),
            error_area,
        );
    }
}

fn render_edit_popup(app: &mut App, frame: &mut Frame, area: Rect) {
    let Some(edit) = &app.settings.editing else {
        return;
    };
    let Some(field) = app.settings.fields.get(edit.field) else {
        return;
    };
    let width = area.width.saturating_sub(6).clamp(44, 72);
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + area.height.saturating_sub(8) / 2,
        width,
        8,
    );
    frame.render_widget(Clear, popup);
    let block = panel(&format!("Edit {}", field.label), true);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let allow_cursor = !app.help_open && app.confirm.is_none();
    let mask = field.kind.is_secret();
    let y = inner.y + 1;
    let before = edit.input.text_before_cursor(mask);
    let value = if mask {
        "•".repeat(edit.input.chars().len())
    } else {
        util::truncate(&edit.input.value(), inner.width.saturating_sub(4) as usize)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("▸ ", Style::default().fg(ACCENT).bold()),
            Span::styled(value, Style::default().fg(TEXT)),
        ]))
        .style(Style::default().bg(PANEL_ALT)),
        Rect::new(inner.x, y, inner.width, 1),
    );
    if allow_cursor {
        let x = inner.x + 2 + util::display_width(&before) as u16;
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), y));
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            util::truncate(
                "Enter commits · Esc cancels · Ctrl+U clears",
                inner.width as usize,
            ),
            Style::default().fg(MUTED),
        ))),
        Rect::new(inner.x, y + 2, inner.width, 1),
    );
    if let Some(error) = &app.settings.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                util::truncate(error, inner.width as usize),
                Style::default().fg(ERROR),
            ))),
            Rect::new(inner.x, y + 3, inner.width, 1),
        );
    }
}

// -------------------------------------------------------------------------
// Overlays
// -------------------------------------------------------------------------

fn render_confirm(confirm: &crate::app::ConfirmDialog, frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(8).clamp(40, 68);
    let height = (confirm.lines.len() as u16 + 5).min(area.height);
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = panel(&confirm.title, true);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut lines: Vec<Line> = confirm
        .lines
        .iter()
        .map(|line| {
            Line::from(Span::styled(
                util::truncate(line, inner.width as usize),
                Style::default().fg(TEXT),
            ))
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[y]", Style::default().fg(SUCCESS).bold()),
        Span::styled(" yes    ", Style::default().fg(MUTED)),
        Span::styled("[n]", Style::default().fg(ERROR).bold()),
        Span::styled(" no", Style::default().fg(MUTED)),
    ]));
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

fn render_help(app: &App, frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = vec![
        help_section("Global"),
        help_line("F1 / ?", "open this help"),
        help_line("F2", "settings"),
        help_line("F5", "reconnect to Soulseek"),
        help_line("Ctrl+Q", "quit"),
    ];

    match app.screen {
        crate::app::Screen::Setup => {
            lines.push(help_section("Setup"));
            lines.push(help_line("Tab", "next field"));
            lines.push(help_line("Enter", "continue / save"));
            lines.push(help_line("Ctrl+S", "save credentials"));
            lines.push(help_line("Esc", "quit"));
        }
        crate::app::Screen::Search => {
            lines.push(help_section("Search"));
            lines.push(help_line("Tab", "switch between fields"));
            lines.push(help_line("Enter", "search / open release"));
            lines.push(help_line("↑ ↓", "choose a release"));
            lines.push(help_line("/", "edit the album"));
            lines.push(help_line("r", "search again"));
            lines.push(help_line("s", "settings"));
            lines.push(help_line("Esc", "cancel / leave input"));
        }
        crate::app::Screen::Tracks => {
            lines.push(help_section("Tracklist"));
            lines.push(help_line("↑ ↓", "scroll"));
            lines.push(help_line("d / Enter", "start download"));
            lines.push(help_line("f", "toggle fingerprint check"));
            lines.push(help_line("b", "back to search"));
            lines.push(help_line("s", "settings"));
        }
        crate::app::Screen::Download => {
            lines.push(help_section("Download"));
            lines.push(help_line("↑ ↓", "select track"));
            lines.push(help_line("Tab", "switch tracks / activity"));
            lines.push(help_line("x", "cancel selected track"));
            lines.push(help_line("r", "retry failed tracks"));
            lines.push(help_line("n", "new search"));
            lines.push(help_line("o", "open album folder"));
            lines.push(help_line("Esc", "back when finished"));
            lines.push(help_line("q", "quit"));
        }
        crate::app::Screen::Settings => {
            lines.push(help_section("Settings"));
            lines.push(help_line("↑ ↓", "select a field"));
            lines.push(help_line("Enter", "edit / toggle"));
            lines.push(help_line("Ctrl+S", "save and go back"));
            lines.push(help_line("Esc", "discard and go back"));
        }
    }
    let width = area.width.saturating_sub(6).clamp(42, 64);
    let height = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(6);
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = panel("Keys", true);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn help_section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(ACCENT).bold(),
    ))
}

fn help_line(key: &str, action: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<10}"), Style::default().fg(WARNING)),
        Span::styled(action.to_string(), Style::default().fg(TEXT)),
    ])
}

// -------------------------------------------------------------------------
// Shared widgets
// -------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_input_row(
    frame: &mut Frame,
    area: Rect,
    y: u16,
    label: &str,
    input: &TextInput,
    focused: bool,
    mask: bool,
    placeholder: &str,
    allow_cursor: bool,
) {
    if y >= area.bottom() {
        return;
    }
    let row = Rect::new(area.x, y, area.width, 1);
    if focused {
        frame.render_widget(Block::default().style(Style::default().bg(PANEL_ALT)), row);
    }
    let marker = if focused { "▸ " } else { "  " };
    let label_width = 18usize;
    let value = if mask {
        "•".repeat(input.chars().len())
    } else {
        input.value()
    };
    let display_value = if value.is_empty() && !focused {
        Span::styled(
            util::truncate(placeholder, area.width.saturating_sub(22) as usize),
            Style::default().fg(BORDER),
        )
    } else {
        Span::styled(
            util::truncate(&value, area.width.saturating_sub(22) as usize),
            Style::default().fg(TEXT),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                marker,
                Style::default()
                    .fg(if focused { ACCENT } else { BORDER })
                    .bold(),
            ),
            Span::styled(
                util::fit(&format!("{label}:"), label_width),
                Style::default().fg(if focused { ACCENT } else { MUTED }),
            ),
            display_value,
        ])),
        row,
    );
    if focused && allow_cursor {
        let before = input.text_before_cursor(mask);
        let x = area.x + 2 + label_width as u16 + util::display_width(&before) as u16;
        frame.set_cursor_position((x.min(row.right().saturating_sub(1)), y));
    }
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let width = (u32::from(area.width) * u32::from(percent_x) / 100) as u16;
    let width = width.max(20).min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Screen, SearchFocus, TracksView};
    use crate::cli::Args;
    use crate::config::Config;
    use crate::musicbrainz::{ReleaseCandidate, ReleaseInfo, TrackInfo};
    use crate::session::TrackStatus;
    use clap::Parser;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let args = Args::parse_from(["soulsucker"]);
        App::new(
            Config::default(),
            Config::default(),
            "/tmp/soulsucker-test.toml".into(),
            &args,
            None,
        )
    }

    fn render_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).map_or(" ", |cell| cell.symbol()));
            }
            text.push('\n');
        }
        text
    }

    fn release() -> ReleaseInfo {
        ReleaseInfo {
            mbid: "release-id".into(),
            release_group_mbid: "group-id".into(),
            title: "Test Album".into(),
            artist: "Test Artist".into(),
            date: "2020-01-02".into(),
            country: "GB".into(),
            status: "Official".into(),
            disambiguation: String::new(),
            primary_type: "Album".into(),
            tracks: vec![
                TrackInfo {
                    index: 0,
                    disc: 1,
                    position: 1,
                    title: "First Song".into(),
                    artist: "Test Artist".into(),
                    length_ms: Some(180_000),
                    recording_mbid: "rec-1".into(),
                    track_mbid: "track-1".into(),
                },
                TrackInfo {
                    index: 1,
                    disc: 1,
                    position: 2,
                    title: "Second Song".into(),
                    artist: "Test Artist".into(),
                    length_ms: Some(240_000),
                    recording_mbid: "rec-2".into(),
                    track_mbid: "track-2".into(),
                },
            ],
        }
    }

    #[test]
    fn setup_and_search_screens_render() {
        let mut app = app();
        let setup = render_text(&mut app, 110, 34);
        assert!(setup.contains("Welcome to soulsucker"));

        app.screen = Screen::Search;
        app.search.focus = SearchFocus::Results;
        app.search.results = vec![ReleaseCandidate {
            mbid: "release-id".into(),
            title: "Test Album".into(),
            artist: "Test Artist".into(),
            date: "2020-01-02".into(),
            country: "GB".into(),
            status: "Official".into(),
            disambiguation: String::new(),
            track_count: 2,
            primary_type: "Album".into(),
            secondary_types: Vec::new(),
            formats: vec!["CD".into()],
            score: 0,
        }];
        app.search.list_state.select(Some(0));
        let search = render_text(&mut app, 110, 34);
        assert!(search.contains("Search MusicBrainz"));
        assert!(search.contains("Test Album"));
        assert!(search.contains("recommended"));
    }

    #[test]
    fn tracks_download_and_settings_screens_render() {
        let mut app = app();
        let release = release();
        let tracks = TracksView::build(release.clone(), &app.cfg);
        let planned = tracks.tracks.clone();
        let album_dir = tracks.album_dir.clone();
        app.tracks = Some(tracks);
        app.screen = Screen::Tracks;
        let tracks_text = render_text(&mut app, 110, 34);
        assert!(tracks_text.contains("Tracklist"));
        assert!(tracks_text.contains("First Song"));

        let mut download = crate::app::DownloadView::new(1, &release, album_dir, &planned, false);
        download.phase = DownloadPhase::Downloading;
        download.rows[0].status = TrackStatus::Downloading;
        download.rows[0].bytes = 12_000_000;
        download.rows[0].total = 24_000_000;
        download.rows[0].speed = 1_500_000.0;
        download.rows[0].source = "sharer".into();
        download.rows[1].status = TrackStatus::Done;
        download.rows[1].bytes = 20_000_000;
        download.rows[1].total = 20_000_000;
        download.rows[1].note = "duration 4:00".into();
        app.download = Some(download);
        app.screen = Screen::Download;
        let download_text = render_text(&mut app, 110, 34);
        assert!(download_text.contains("Download"));
        assert!(download_text.contains("First Song"));
        assert!(download_text.contains("Activity"));

        app.screen = Screen::Settings;
        let settings_text = render_text(&mut app, 110, 34);
        assert!(settings_text.contains("Settings"));
        assert!(settings_text.contains("Parallel downloads"));

        app.help_open = true;
        let help_text = render_text(&mut app, 110, 34);
        assert!(help_text.contains("Keys"));
    }
}
