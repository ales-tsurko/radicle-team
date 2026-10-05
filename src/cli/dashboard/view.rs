//! Rounded panels, concise queue rows, and inspectable full contexts.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
};

use crate::{
    agent::Role,
    cli::{
        dashboard::model::{Detail, EventLabel, Panel, Screen},
        display::Display,
    },
    delivery::{Record, State},
    manifest::QueuePosition,
    queue::{ArtifactRole, Task},
};

pub(super) struct View<'a> {
    screen: &'a mut Screen,
    theme: Theme,
}

impl<'a> View<'a> {
    pub(super) fn new(screen: &'a mut Screen) -> Self {
        Self {
            screen,
            theme: Theme::new(),
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame<'_>) {
        self.screen.areas.clear();
        self.screen.overlay = None;
        self.screen.text.begin_frame();
        let area = frame.area();
        let narrow = area.width < 100 || area.height < 24;
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        self.screen.header = header;
        self.header(frame, header);
        if narrow {
            self.compact(frame, body);
        } else {
            self.wide(frame, body);
        }
        frame.render_widget(
            Paragraph::new(self.screen.hint(footer.width)).style(match &self.screen.notice {
                Some(Err(_)) => self.theme.state("failed"),
                Some(Ok(_)) => self.theme.state("handled"),
                None => self.theme.dim(),
            }),
            footer,
        );
        if let Some(save) = &self.screen.saving {
            let text = format!(
                "{} log lines\n\nPath\n{}▏\n\nEnter save · Esc cancel · Ctrl+U clear \
                 path\nExisting files are kept.",
                save.contents.lines().count(),
                save.path,
            );
            self.overlay(frame, "Save log", text);
        } else if self.screen.help {
            self.overlay(frame, "Keyboard", Self::help());
        } else if self.screen.detail.is_some() {
            let (title, text) = self.detail();
            self.overlay(frame, &title, text);
        }
        self.screen.text.render(frame.buffer_mut());
    }

    fn header(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let phase = self.screen.phase().to_string();
        let snapshot = self.screen.data.snapshot.as_ref();
        let uptime = snapshot.map_or_else(
            || "starting".to_owned(),
            |value| Display::age(value.started),
        );
        let fetched = snapshot
            .and_then(|value| value.fetched)
            .map_or_else(|| "not yet".to_owned(), Display::age);
        let mut text = vec![
            Line::from(vec![
                Span::styled("rad-team", self.theme.accent().bold()),
                Span::styled(
                    format!(" · {}   ", self.screen.data.member),
                    Style::new().bold(),
                ),
                Span::styled(
                    format!("{} {phase}", Theme::symbol(&phase)),
                    self.theme.state(&phase).bold(),
                ),
                Span::styled(format!("   Up: {uptime}"), self.theme.dim()),
            ]),
            Line::styled(
                format!(
                    "{}   ·   Last fetch: {fetched}",
                    self.screen.data.repository
                ),
                self.theme.dim(),
            ),
        ];
        let warning = self
            .screen
            .refresh_error
            .as_ref()
            .or_else(|| snapshot.and_then(|value| value.sync_error.as_ref()))
            .or_else(|| snapshot.and_then(|value| value.failure.as_ref()));
        if let Some(warning) = warning {
            text.push(Line::styled(
                format!("! {warning}"),
                self.theme.state("failed"),
            ));
        } else if self.screen.refreshed.elapsed().as_secs() > 3 {
            text.push(Line::styled(
                format!(
                    "! Display update delayed: {}s",
                    self.screen.refreshed.elapsed().as_secs()
                ),
                self.theme.state("waiting"),
            ));
        }
        frame.render_widget(Paragraph::new(text), area);
        let repository = self.screen.data.repository.to_string();
        self.screen.text.bind(
            Rect::new(area.x, area.y + 1, area.width, 1),
            repository.clone(),
            repository,
        );
    }

    fn wide(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let [status, queues, logs] = Layout::vertical([
            Constraint::Length(6),
            Constraint::Fill(1),
            Constraint::Fill(1),
        ])
        .areas(area);
        let [tasks, inbox] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(queues);
        self.screen
            .areas
            .extend(Panel::ALL.into_iter().zip([status, tasks, inbox, logs]));
        self.status(frame, status);
        self.queue(frame, tasks, Panel::Tasks);
        self.queue(frame, inbox, Panel::Inbox);
        self.logs(frame, logs);
    }

    fn compact(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let [tabs, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        self.screen.areas.insert(self.screen.focus, body);
        let labels = Panel::ALL.map(|panel| {
            let suffix = match panel {
                Panel::Tasks => format!(" ({})", self.screen.data.tasks.len()),
                Panel::Inbox => format!(" ({})", self.screen.data.deliveries.len()),
                Panel::Status | Panel::Logs => String::new(),
            };
            format!("{}{suffix}", panel.title())
        });
        frame.render_widget(
            Tabs::new(labels)
                .select(self.screen.focus.index())
                .highlight_style(self.theme.accent().bold()),
            tabs,
        );
        match self.screen.focus {
            Panel::Status => self.status(frame, body),
            Panel::Tasks | Panel::Inbox => self.queue(frame, body, self.screen.focus),
            Panel::Logs => self.logs(frame, body),
        }
    }

    fn block(&self, panel: Panel, title: String) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(if self.screen.focus == panel {
                self.theme.accent()
            } else {
                self.theme.dim()
            })
            .title(Line::styled(format!(" {title} "), Style::new().bold()))
    }

    fn status(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = [Role::Coder, Role::Talker, Role::Chatter]
            .map(|role| self.role(role))
            .to_vec();
        lines.push(Line::styled(
            self.screen.data.did.map_or_else(
                || "DID: loading identity…".to_owned(),
                |did| format!("DID: {did}"),
            ),
            self.theme.dim(),
        ));
        frame.render_widget(
            Paragraph::new(lines).block(self.block(Panel::Status, "Status".to_owned())),
            area,
        );
        let inner = area.inner(Margin::new(1, 1));
        if let Some(did) = self.screen.data.did {
            self.screen.text.bind(
                Rect::new(inner.x, inner.y + 3, inner.width, 1).intersection(inner),
                did.to_string(),
                did.to_string(),
            );
        }
        for (row, role) in [Role::Coder, Role::Talker, Role::Chatter]
            .into_iter()
            .enumerate()
        {
            let activity = self
                .screen
                .data
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.roles.get(&role))
                .and_then(|status| status.activity.as_ref());
            if let Some(activity) = activity {
                let row =
                    Rect::new(inner.x, inner.y + row as u16, inner.width, 1).intersection(inner);
                let source = activity.source.clone();
                let task_visible = activity
                    .task
                    .is_some_and(|id| self.screen.data.tasks.iter().any(|(task, _)| task.id == id));
                if let Some(id) = activity.task {
                    self.screen.text.bind(row, format!("#{id}"), id.to_string());
                }
                if !task_visible {
                    self.bind_source(row, &source);
                }
            }
        }
    }

    fn role(&self, role: Role) -> Line<'static> {
        let status = self
            .screen
            .data
            .snapshot
            .as_ref()
            .and_then(|value| value.roles.get(&role));
        let state = self.screen.data.role_state(role);
        let mut spans = vec![
            Span::styled(
                format!("{} ", Theme::symbol(state)),
                self.theme.state(state),
            ),
            Span::styled(format!("{:<8}", role.to_string()), Style::new().bold()),
        ];
        if let Some(status) = status
            && status.enabled != Some(false)
            && let Some(activity) = &status.activity
        {
            let subject = activity.task.and_then(|id| {
                self.screen
                    .data
                    .tasks
                    .iter()
                    .find(|(task, _)| task.id == id)
            });
            let brief = subject.map_or_else(
                || Self::short_subject(&activity.source),
                |(task, _)| format!("#{}: {}", task.id, Self::single_line(&task.brief)),
            );
            spans.push(Span::styled(
                format!("{state} · {brief}"),
                self.theme.state(state),
            ));
            if let Some(started) = status.started {
                spans.push(Span::styled(
                    format!(" · {}", Display::age(started)),
                    self.theme.dim(),
                ));
            }
            if let Some(progress) = status.progress {
                spans.push(Span::styled(
                    format!(" · progress {}", Display::age(progress)),
                    self.theme.dim(),
                ));
            }
        } else {
            spans.push(Span::styled(state, self.theme.state(state)));
        }
        Line::from(spans)
    }

    fn queue(&mut self, frame: &mut Frame<'_>, area: Rect, panel: Panel) {
        let (items, title, empty): (Vec<_>, _, _) = match panel {
            Panel::Tasks => {
                let items = self
                    .screen
                    .task_indices()
                    .into_iter()
                    .filter_map(|index| self.screen.data.tasks.get(index))
                    .map(|(task, taken)| self.task_row(task, *taken))
                    .collect();
                let empty = if self
                    .screen
                    .filters
                    .get(&Panel::Tasks)
                    .is_none_or(String::is_empty)
                {
                    "No unfinished tasks."
                } else {
                    "No matching tasks."
                };
                (
                    items,
                    format!("Tasks · {}", self.screen.data.tasks.len()),
                    empty,
                )
            }
            Panel::Inbox => {
                let items = self
                    .screen
                    .inbox_indices()
                    .into_iter()
                    .filter_map(|index| self.screen.data.deliveries.get(index))
                    .map(|record| self.inbox_row(record))
                    .collect();
                let waiting = self
                    .screen
                    .data
                    .deliveries
                    .iter()
                    .filter(|record| record.state == State::Queued)
                    .count();
                (
                    items,
                    format!("Inbox · {waiting} waiting"),
                    "No matching notifications.",
                )
            }
            Panel::Status | Panel::Logs => return,
        };
        let block = self.block(panel, title);
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(empty).style(self.theme.dim()).block(block),
                area,
            );
            return;
        }
        let focused = self.screen.focus == panel;
        if focused {
            self.screen.page = usize::from(area.height.saturating_sub(2) / 2).max(1);
        }
        let list = List::new(items)
            .block(block)
            .highlight_symbol(if focused { "› " } else { "  " })
            .highlight_style(self.selection_style(focused));
        let selection = if panel == Panel::Tasks {
            &mut self.screen.tasks.state
        } else {
            &mut self.screen.inbox.state
        };
        frame.render_stateful_widget(list, area, selection);
        let offset = selection.offset();
        let inner = area.inner(Margin::new(1, 1));
        let indices = if panel == Panel::Tasks {
            self.screen.task_indices()
        } else {
            self.screen.inbox_indices()
        };
        for (row, index) in indices
            .into_iter()
            .skip(offset)
            .take(usize::from(inner.height).div_ceil(2))
            .enumerate()
        {
            let heading =
                Rect::new(inner.x, inner.y + row as u16 * 2, inner.width, 1).intersection(inner);
            let metadata = Rect::new(inner.x, heading.y + 1, inner.width, 1).intersection(inner);
            if panel == Panel::Tasks {
                let Some((task, _)) = self.screen.data.tasks.get(index) else {
                    continue;
                };
                let id = task.id;
                let source = task.source.to_string();
                self.screen
                    .text
                    .bind(heading, format!("#{id}"), id.to_string());
                self.bind_source(metadata, &source);
            } else {
                let Some(record) = self.screen.data.deliveries.get(index) else {
                    continue;
                };
                let subject = record.activity.subject.to_string();
                if let Some(id) = record.task {
                    self.screen
                        .text
                        .bind(heading, format!("#{id}"), id.to_string());
                }
                if self.screen.data.did != Some(record.activity.author) {
                    let author = record.activity.author.to_string();
                    self.screen
                        .text
                        .bind(metadata, Self::short_identity(&author), author);
                }
                self.bind_source(heading, &subject);
            }
        }
    }

    fn bind_source(&mut self, area: Rect, source: &str) {
        let value = source.split_once(':').map_or(source, |(_, id)| id);
        self.screen
            .text
            .bind(area, value.chars().take(7).collect(), value.to_owned());
        self.screen
            .text
            .bind(area, Self::short_subject(source), value.to_owned());
    }

    fn task_row(&self, task: &Task, taken: bool) -> ListItem<'static> {
        let state = self.screen.data.task_state(task, taken);
        self.row(
            state,
            format!("#{} {}", task.id, Self::single_line(&task.brief)),
            format!(
                "{state} · {} · {}",
                Self::priority(task.position),
                Self::short_subject(&task.source.to_string())
            ),
        )
    }

    fn inbox_row(&self, record: &Record) -> ListItem<'static> {
        let state = if !self.screen.data.live && record.state == State::Processing {
            "interrupted".to_owned()
        } else {
            record.state.to_string()
        };
        let author = if self.screen.data.did == Some(record.activity.author) {
            self.screen.data.member.to_string()
        } else {
            Self::short_identity(&record.activity.author.to_string())
        };
        let task = record.task.map_or(String::new(), |id| format!(" · #{id}"));
        self.row(
            &state,
            format!(
                "{} · {}{task}",
                EventLabel::of(record),
                Self::short_subject(&record.activity.subject.to_string())
            ),
            format!("{state} · {author} · {}", Display::age(record.updated)),
        )
    }

    fn row(&self, state: &str, heading: String, metadata: String) -> ListItem<'static> {
        ListItem::new(vec![
            Line::from(vec![
                Span::styled(
                    format!("{} ", Theme::symbol(state)),
                    self.theme.state(state),
                ),
                Span::styled(heading, Style::new().bold()),
            ]),
            Line::styled(format!("  {metadata}"), self.theme.dim()),
        ])
    }

    fn selection_style(&self, focused: bool) -> Style {
        if focused {
            self.theme.accent().bold().add_modifier(Modifier::REVERSED)
        } else {
            self.theme.dim()
        }
    }

    fn logs(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.screen.logs.page = usize::from(area.height.saturating_sub(2)).max(1);
        let mode = if self.screen.logs.following {
            "following".to_owned()
        } else {
            let unseen = self.screen.logs.unseen();
            format!(
                "Paused · {unseen} new {}",
                if unseen == 1 { "entry" } else { "entries" }
            )
        };
        let visible = self.screen.logs.visible();
        let start = self.screen.logs.start(&visible);
        let lines: Vec<_> = visible
            .iter()
            .skip(start)
            .take(self.screen.logs.page)
            .map(|line| line.text.clone())
            .collect();
        let inner = area.inner(Margin::new(1, 1));
        for (row, line) in lines.iter().enumerate() {
            let row = Rect::new(inner.x, inner.y + row as u16, inner.width, 1).intersection(inner);
            let value = line.to_string();
            self.screen
                .text
                .bind(row, value.clone(), format!("{value}\n"));
            self.screen.text.bind_text(row, &value);
        }
        let text = if lines.is_empty() {
            Text::from(Line::styled(
                if self.screen.logs.filter.is_empty() {
                    "No log entries."
                } else {
                    "No matching log entries."
                },
                self.theme.dim(),
            ))
        } else {
            Text::from(lines)
        };
        frame.render_widget(
            Paragraph::new(text).block(self.block(Panel::Logs, format!("Logs · {mode}"))),
            area,
        );
    }

    fn overlay(&mut self, frame: &mut Frame<'_>, title: &str, text: String) {
        let popup = Self::popup(frame.area());
        self.screen.overlay = Some(popup);
        self.screen.page = usize::from(popup.height.saturating_sub(2)).max(1);
        self.screen
            .text
            .bind_text(Rect::new(popup.x, popup.y, popup.width, 1), title);
        self.screen
            .text
            .bind_text(popup.inner(Margin::new(1, 1)), &text);
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let maximum = paragraph
            .line_count(popup.width.saturating_sub(2))
            .saturating_sub(usize::from(popup.height.saturating_sub(2)));
        self.screen.detail_scroll = self
            .screen
            .detail_scroll
            .min(u16::try_from(maximum).unwrap_or(u16::MAX));
        frame.render_widget(Clear, popup);
        frame.render_widget(
            paragraph.scroll((self.screen.detail_scroll, 0)).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(self.theme.accent())
                    .title(Line::styled(format!(" {title} "), Style::new().bold())),
            ),
            popup,
        );
    }

    fn popup(area: Rect) -> Rect {
        let width = area.width.saturating_sub(4).min(100);
        let height = area.height.saturating_sub(4);
        Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        )
    }

    fn detail(&self) -> (String, String) {
        match &self.screen.detail {
            Some(Detail::Status) => ("Member status".to_owned(), self.status_detail()),
            Some(Detail::Task(id)) => (format!("Task #{id}"), self.task_detail(id)),
            Some(Detail::Delivery(id)) => ("Notification".to_owned(), self.delivery_detail(id)),
            None => (String::new(), String::new()),
        }
    }

    fn status_detail(&self) -> String {
        let data = &self.screen.data;
        let mut lines = vec![
            format!("Member: {}", data.member),
            format!("Phase: {}", self.screen.phase()),
            format!("Repository: {}", data.repository),
            format!("Home: {}", data.home.path().display()),
            format!(
                "DID: {}",
                data.did
                    .map_or_else(|| "not yet loaded".to_owned(), |did| did.to_string())
            ),
        ];
        if let Some(snapshot) = &data.snapshot {
            lines.push(format!(
                "Started: {}\nLast activity: {}",
                Display::age(snapshot.started),
                Display::age(snapshot.updated)
            ));
            if let Some(fetched) = snapshot.fetched {
                lines.push(format!("Last successful fetch: {}", Display::age(fetched)));
            }
            for role in Role::ALL {
                if let Some(status) = snapshot.roles.get(&role) {
                    lines.push(format!("\n{role}: {}", data.role_state(role)));
                    if let Some(activity) = &status.activity {
                        lines.push(format!("Source: {}", activity.source));
                        if let Some(task) = activity.task {
                            lines.push(format!("Task: #{task}"));
                        }
                    }
                    if let Some(started) = status.started {
                        lines.push(format!("Started: {}", Display::age(started)));
                    }
                    if let Some(progress) = status.progress {
                        lines.push(format!("Last progress: {}", Display::age(progress)));
                    }
                }
            }
            if let Some(error) = &snapshot.sync_error {
                lines.push(format!("\nSync error: {error}"));
            }
            if let Some(error) = &snapshot.failure {
                lines.push(format!("\nLast error: {error}"));
            }
        }
        if let Some(error) = &self.screen.refresh_error {
            lines.push(format!("\nDisplay refresh error: {error}"));
        }
        lines.join("\n")
    }

    fn task_detail(&self, id: &str) -> String {
        let Some((task, taken)) = self
            .screen
            .data
            .tasks
            .iter()
            .find(|(task, _)| task.id.to_string() == id)
        else {
            return "This task has finished or been cancelled; its context is no longer retained."
                .to_owned();
        };
        let mut lines = vec![
            format!("Task: #{}", task.id),
            task.brief.clone(),
            format!(
                "\nState: {}\nPriority: {}\nSource: {}",
                self.screen.data.task_state(task, *taken),
                Self::priority(task.position),
                task.source
            ),
            format!("\nSpecification\n{}", task.context.specification),
        ];
        if task.context.cancelled {
            lines.push("Cancellation requested".to_owned());
        }
        for artifact in &task.context.artifacts {
            lines.push(format!(
                "\n{} ({})\n{}",
                artifact.subject,
                Self::artifact_role(artifact.role),
                artifact.purpose
            ));
        }
        for message in &task.context.messages {
            lines.push(format!(
                "\n{}\nMessage: {}\n{}",
                message.subject, message.entry, message.purpose
            ));
            if let Some(revision) = &message.revision {
                lines.push(format!("Revision: {revision}"));
            }
        }
        if !task.context.notes.is_empty() {
            lines.push(format!("\nNotes\n{}", task.context.notes.join("\n")));
        }
        lines.join("\n")
    }

    fn delivery_detail(&self, id: &str) -> String {
        let Some(record) = self
            .screen
            .data
            .deliveries
            .iter()
            .find(|record| record.id() == id)
        else {
            return "This notification is no longer available.".to_owned();
        };
        let state = if !self.screen.data.live && record.state == State::Processing {
            "interrupted (last recorded: processing)".to_owned()
        } else {
            record.state.to_string()
        };
        let mut lines = vec![
            format!("Event: {}\nState: {}", EventLabel::of(record), state),
            format!(
                "Subject: {}\nAuthor: {}",
                record.activity.subject, record.activity.author
            ),
            format!(
                "Operation: {}\nAction: {}",
                record.activity.operation, record.activity.action
            ),
            format!(
                "Received: {}\nLast observed: {}",
                Display::age(record.received / 1_000),
                Display::age(record.updated)
            ),
        ];
        if let Some(task) = record.task {
            lines.push(format!("Task: #{task}"));
        }
        if let Some(error) = &record.error {
            lines.push(format!("\nReason: {error}"));
        }
        if let Ok(event) = serde_json::to_string_pretty(&record.activity.kind) {
            lines.push(format!("\nEvent references\n{event}"));
        }
        lines.join("\n")
    }

    fn help() -> String {
        "Tab / Shift+Tab    Next / previous panel\ns / t / i / l      Status / tasks / inbox / logs\n↑↓ or j/k          Select a row or scroll a line\nWheel / touchpad   Scroll the panel under the pointer\nPgUp / PgDn        Scroll one page\nHome / End         First / last row\nEnter              Inspect selected task, notification, or status\nClick an ID        Select its complete value, even when shortened\nDrag               Select text in any panel or detail\ny / c              Copy selected text\n/                  Filter the focused list or logs\nEsc                Clear selection / filter or close details\n\nLogs\nHome               Oldest retained entry\nEnd                Latest entry and resume following\nSpace              Pause / resume following\ny                  Copy selection, or all filtered log lines\nw                  Save selection, or all filtered log lines\nScrolling          Pause following; new entries remain buffered\n\n?                  Show / close this help\nq / Ctrl+C         Gracefully stop the member\n\nStatus, tasks, and inbox refresh every second.\nLogs retain the latest 2,000 lines."
            .to_owned()
    }

    fn single_line(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn short_subject(text: &str) -> String {
        text.split_once(':').map_or_else(
            || text.chars().take(24).collect(),
            |(kind, id)| format!("{kind}:{}", id.chars().take(7).collect::<String>()),
        )
    }

    fn short_identity(text: &str) -> String {
        let key = text.strip_prefix("did:key:").unwrap_or(text);
        let prefix: String = key.chars().take(7).collect();
        let suffix: String = key
            .chars()
            .rev()
            .take(7)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("{prefix}…{suffix}")
    }

    const fn priority(position: QueuePosition) -> &'static str {
        match position {
            QueuePosition::Front => "first",
            QueuePosition::Next => "next",
            QueuePosition::End => "normal",
        }
    }

    const fn artifact_role(role: ArtifactRole) -> &'static str {
        match role {
            ArtifactRole::Origin => "origin",
            ArtifactRole::Implementation => "implementation",
            ArtifactRole::Review => "review",
            ArtifactRole::Produced => "produced",
            ArtifactRole::Supporting => "supporting",
        }
    }
}

struct Theme {
    colors: bool,
}

impl Theme {
    fn new() -> Self {
        Self {
            colors: std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
        }
    }

    fn color(&self, color: Color) -> Style {
        if self.colors {
            Style::new().fg(color)
        } else {
            Style::new()
        }
    }

    fn accent(&self) -> Style {
        self.color(Color::Cyan)
    }

    fn dim(&self) -> Style {
        Style::new().dim()
    }

    fn state(&self, state: &str) -> Style {
        match state {
            "failed" => self.color(Color::Red),
            "running" | "active" | "processing" | "handled" => self.color(Color::Green),
            "waiting" | "interrupted" | "waiting for manifest approval" | "stopping" => {
                self.color(Color::Yellow)
            }
            "starting" | "synchronizing" | "initializing agents" | "queued" => self.accent(),
            _ => self.dim(),
        }
    }

    fn symbol(state: &str) -> &'static str {
        match state {
            "failed" | "interrupted" => "!",
            "running" | "active" | "processing" => "●",
            "handled" => "✓",
            "disabled" | "ignored" => "─",
            _ => "○",
        }
    }
}
