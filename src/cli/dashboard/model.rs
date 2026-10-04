//! Keyboard navigation over the existing member inspection data.

use std::{collections::BTreeMap, time::Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::{
    cli::dashboard::logs::LogView,
    delivery::{Record, State},
    status::{Inspection, Phase},
    watcher::{Change, Delivery, Kind},
};

#[derive(Debug)]
pub(super) struct Screen {
    pub(super) data: Inspection,
    pub(super) focus: Panel,
    pub(super) tasks: Selection,
    pub(super) inbox: Selection,
    pub(super) logs: LogView,
    pub(super) detail: Option<Detail>,
    pub(super) detail_scroll: u16,
    pub(super) help: bool,
    pub(super) editing: bool,
    pub(super) filters: BTreeMap<Panel, String>,
    pub(super) stopping: bool,
    pub(super) refreshed: Instant,
    pub(super) refresh_error: Option<String>,
    pub(super) page: usize,
    starting: bool,
}

impl Screen {
    pub(super) fn new(data: Inspection) -> Self {
        let mut screen = Self {
            data,
            focus: Panel::Tasks,
            tasks: Selection::default(),
            inbox: Selection::default(),
            logs: LogView::default(),
            detail: None,
            detail_scroll: 0,
            help: false,
            editing: false,
            filters: Default::default(),
            stopping: false,
            refreshed: Instant::now(),
            refresh_error: None,
            page: 1,
            starting: true,
        };
        screen.sort_inbox();
        screen.reconcile();
        screen
    }

    pub(super) fn refresh(&mut self, data: Inspection) {
        self.starting &= !data.live;
        self.data = data;
        self.refreshed = Instant::now();
        self.refresh_error = None;
        self.sort_inbox();
        self.reconcile();
    }

    pub(super) fn phase(&self) -> Phase {
        if self.stopping {
            Phase::Stopping
        } else if self.starting {
            Phase::Starting
        } else {
            self.data.phase()
        }
    }

    fn sort_inbox(&mut self) {
        self.data.deliveries.sort_by_key(|record| {
            let attention = match record.state {
                State::Failed => 0,
                State::Processing => 1,
                State::Queued => 2,
                State::Handled => 3,
                State::Ignored => 4,
            };
            (attention, std::cmp::Reverse(record.received))
        });
    }

    pub(super) fn task_indices(&self) -> Vec<usize> {
        Self::matching(
            &self.data.tasks,
            self.filters.get(&Panel::Tasks),
            |(task, _)| format!("#{} {} {}", task.id, task.brief, task.source),
        )
    }

    pub(super) fn inbox_indices(&self) -> Vec<usize> {
        Self::matching(
            &self.data.deliveries,
            self.filters.get(&Panel::Inbox),
            |record| {
                format!(
                    "{} {} {} {} {} {}",
                    record.state,
                    EventLabel::of(record),
                    record.activity.subject,
                    record.activity.author,
                    record.activity.operation,
                    record.task.map_or(String::new(), |id| format!("#{id}")),
                )
            },
        )
    }

    fn matching<T>(
        items: &[T],
        filter: Option<&String>,
        describe: impl Fn(&T) -> String,
    ) -> Vec<usize> {
        let query = filter.map_or("", String::as_str).to_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| describe(item).to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect()
    }

    pub(super) fn task_ids(&self) -> Vec<String> {
        self.task_indices()
            .into_iter()
            .filter_map(|index| self.data.tasks.get(index))
            .map(|(task, _)| task.id.to_string())
            .collect()
    }

    pub(super) fn inbox_ids(&self) -> Vec<String> {
        self.inbox_indices()
            .into_iter()
            .filter_map(|index| self.data.deliveries.get(index))
            .map(Record::id)
            .collect()
    }

    fn reconcile(&mut self) {
        self.tasks.reconcile(&self.task_ids());
        self.inbox.reconcile(&self.inbox_ids());
    }

    pub(super) fn filter(&self) -> &str {
        if self.focus == Panel::Logs {
            &self.logs.filter
        } else {
            self.filters.get(&self.focus).map_or("", String::as_str)
        }
    }

    pub(super) fn hint(&self, width: u16) -> String {
        let text = if self.editing {
            format!("/{}▏   Enter apply · Esc clear", self.filter())
        } else if self.help || self.detail.is_some() {
            "↑↓ scroll  PgUp/PgDn page  Esc close  q stop".to_owned()
        } else if width < 60 {
            "Tab panels  ? help  q stop".to_owned()
        } else if self.focus == Panel::Status {
            "Tab panels  Enter details  ? help  q stop".to_owned()
        } else if self.focus == Panel::Logs && width < 100 {
            "Tab panels  ↑↓ scroll  PgUp/Dn page  Space follow  ? help  q stop".to_owned()
        } else if self.focus == Panel::Logs {
            "Tab panels  ↑↓ line  PgUp/PgDn page  Home/End  Space follow  / filter  ? help  q stop"
                .to_owned()
        } else {
            "Tab panels  ↑↓ select  Enter details  / filter  ? help  q stop".to_owned()
        };
        if !self.editing && !self.filter().is_empty() {
            format!("Filter: {} · {text}", self.filter())
        } else {
            text
        }
    }

    fn filter_mut(&mut self) -> &mut String {
        if self.focus == Panel::Logs {
            &mut self.logs.filter
        } else {
            self.filters.entry(self.focus).or_default()
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if self.editing {
            self.edit_filter(key.code);
            return false;
        }
        if key.code == KeyCode::Char('q') {
            return true;
        }
        if self.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                self.help = false;
                self.detail_scroll = 0;
            } else {
                self.scroll_detail(key.code);
            }
            return false;
        }
        if self.detail.is_some() {
            self.scroll_detail(key.code);
            return false;
        }
        self.navigate(key.code);
        false
    }

    fn navigate(&mut self, code: KeyCode) {
        match code {
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.previous(),
            KeyCode::Char('s') => self.focus = Panel::Status,
            KeyCode::Char('t') => self.focus = Panel::Tasks,
            KeyCode::Char('i') => self.focus = Panel::Inbox,
            KeyCode::Char('l') => self.focus = Panel::Logs,
            KeyCode::Char('?') => {
                self.help = true;
                self.detail_scroll = 0;
            }
            KeyCode::Char('/') if self.focus != Panel::Status => self.editing = true,
            KeyCode::Esc => {
                self.filter_mut().clear();
                self.reconcile();
            }
            KeyCode::Enter => self.open_detail(),
            _ => self.scroll_panel(code),
        }
    }

    fn edit_filter(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => self.editing = false,
            KeyCode::Esc => {
                self.editing = false;
                self.filter_mut().clear();
            }
            KeyCode::Backspace => {
                self.filter_mut().pop();
            }
            KeyCode::Char(character) => self.filter_mut().push(character),
            _ => {}
        }
        self.reconcile();
    }

    fn scroll_panel(&mut self, code: KeyCode) {
        if self.focus == Panel::Logs {
            match code {
                KeyCode::Home => self.logs.home(),
                KeyCode::End => self.logs.follow(),
                KeyCode::Char(' ') => self.logs.toggle_follow(),
                _ => {
                    let amount = Self::distance(code, self.logs.page);
                    if amount != 0 {
                        self.logs.scroll(amount);
                    }
                }
            }
            return;
        }
        let amount = Self::distance(code, self.page);
        match self.focus {
            Panel::Tasks => self.tasks.move_by(&self.task_ids(), amount),
            Panel::Inbox => self.inbox.move_by(&self.inbox_ids(), amount),
            Panel::Status | Panel::Logs => {}
        }
    }

    fn open_detail(&mut self) {
        self.detail = match self.focus {
            Panel::Status => Some(Detail::Status),
            Panel::Tasks => self.tasks.id.clone().map(Detail::Task),
            Panel::Inbox => self.inbox.id.clone().map(Detail::Delivery),
            Panel::Logs => None,
        };
        self.detail_scroll = 0;
    }

    fn scroll_detail(&mut self, code: KeyCode) {
        if code == KeyCode::Esc || code == KeyCode::Enter {
            self.detail = None;
            return;
        }
        self.detail_scroll = self
            .detail_scroll
            .saturating_add_signed(Self::distance(code, self.page).clamp(-32_768, 32_767) as i16);
    }

    fn distance(code: KeyCode, page: usize) -> isize {
        let page = isize::try_from(page).unwrap_or(isize::MAX);
        match code {
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::PageUp => -page,
            KeyCode::PageDown => page,
            KeyCode::Home => isize::MIN,
            KeyCode::End => isize::MAX,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Panel {
    Status,
    Tasks,
    Inbox,
    Logs,
}

impl Panel {
    pub(super) const ALL: [Self; 4] = [Self::Status, Self::Tasks, Self::Inbox, Self::Logs];

    pub(super) const fn index(self) -> usize {
        match self {
            Self::Status => 0,
            Self::Tasks => 1,
            Self::Inbox => 2,
            Self::Logs => 3,
        }
    }

    pub(super) const fn title(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Tasks => "Tasks",
            Self::Inbox => "Inbox",
            Self::Logs => "Logs",
        }
    }

    fn next(self) -> Self {
        Self::ALL
            .get((self.index() + 1) % Self::ALL.len())
            .copied()
            .unwrap_or(Self::Status)
    }

    fn previous(self) -> Self {
        Self::ALL
            .get((self.index() + Self::ALL.len() - 1) % Self::ALL.len())
            .copied()
            .unwrap_or(Self::Status)
    }
}

#[derive(Debug, Default)]
pub(super) struct Selection {
    pub(super) id: Option<String>,
    pub(super) state: ListState,
}

impl Selection {
    fn reconcile(&mut self, ids: &[String]) {
        let selected = self
            .id
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id))
            .or_else(|| (!ids.is_empty()).then_some(0));
        self.state.select(selected);
        self.id = selected.and_then(|index| ids.get(index)).cloned();
    }

    fn move_by(&mut self, ids: &[String], amount: isize) {
        self.reconcile(ids);
        if let Some(index) = self.state.selected() {
            let next = index
                .saturating_add_signed(amount)
                .min(ids.len().saturating_sub(1));
            self.state.select(Some(next));
            self.id = ids.get(next).cloned();
        }
    }
}

#[derive(Debug)]
pub(super) enum Detail {
    Status,
    Task(String),
    Delivery(String),
}

pub(super) struct EventLabel;

impl EventLabel {
    pub(super) fn of(record: &Record) -> &'static str {
        match record.activity.kind {
            Kind::Assigned => "assignment",
            Kind::Delivered(Delivery::Mentioned(_)) => "mention",
            Kind::Delivered(Delivery::Replied { .. }) => "reply",
            Kind::Delivered(Delivery::Changed(change)) => Self::change(change),
        }
    }

    pub(super) const fn change(change: Change) -> &'static str {
        match change {
            Change::ReviewSubmitted { .. }
            | Change::ReviewEdited(_)
            | Change::ReviewRedacted(_)
            | Change::ReviewFollowUp { .. } => "review",
            Change::RevisionCreated(_)
            | Change::RevisionEdited(_)
            | Change::RevisionRedacted(_) => "revision",
            Change::CommentCreated(_) | Change::CommentEdited(_) | Change::CommentRedacted(_) => {
                "comment"
            }
            Change::CommentReaction { .. }
            | Change::ReviewReaction { .. }
            | Change::RevisionReaction { .. } => "reaction",
            Change::CommentResolution { .. } | Change::ReviewCommentsResolved => "resolution",
            Change::Unassigned => "unassigned",
            Change::Edited | Change::LabelsChanged => "metadata",
            Change::LifecycleChanged | Change::PatchReady => "lifecycle",
            Change::Merged => "merge",
        }
    }
}
