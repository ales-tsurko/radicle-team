use std::{
    io::Write as _,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter as _;

use crate::{
    agent::{Role, Source},
    cli::dashboard::{
        LogBuffer,
        export::Export,
        logs::{LogLine, LogView},
        model::{Detail, EventLabel, Panel, Screen},
        view::View,
    },
    delivery::{Record, State},
    home::MemberHome,
    manifest::QueuePosition,
    member::Member,
    queue::{Artifact, ArtifactRole, MessageReference, Task, TaskContext},
    status::{Activity as RoleActivity, Inspection, Phase, Snapshot},
    watcher::{Activity, Change, Comment, Delivery, Kind, Subject, Target},
};

struct Fixture;

impl Fixture {
    fn inspection() -> Inspection {
        let mut snapshot = Snapshot::new("dashboard-test".to_owned());
        snapshot.phase = Phase::Running;
        snapshot.fetched = Some(Snapshot::now());
        for status in snapshot.roles.values_mut() {
            status.enabled = Some(true);
        }
        let task = Self::task(2, "Monotonic task IDs", QueuePosition::Front);
        let coder = snapshot.roles.get_mut(&Role::Coder).expect("coder status");
        coder.activity = Some(RoleActivity {
            task: Some(task.id),
            source: task.source.to_string(),
        });
        coder.started = Some(Snapshot::now().saturating_sub(120));
        coder.progress = Some(Snapshot::now().saturating_sub(3));
        let talker = snapshot
            .roles
            .get_mut(&Role::Talker)
            .expect("talker status");
        talker.activity = Some(RoleActivity {
            task: None,
            source: "patch:2222222222222222222222222222222222222222".to_owned(),
        });
        let chatter = snapshot
            .roles
            .get_mut(&Role::Chatter)
            .expect("chatter status");
        chatter.enabled = Some(false);
        Inspection {
            home: MemberHome::at("/tmp/rad-team-dashboard-fixture"),
            member: Member::Nico,
            repository: "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc".parse().expect("RID"),
            did: Some(
                "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C"
                    .parse()
                    .expect("DID"),
            ),
            snapshot: Some(snapshot),
            tasks: vec![
                (task, true),
                (Self::task(3, "Safe undo", QueuePosition::Next), false),
                (Self::task(4, "Search filters", QueuePosition::End), false),
            ],
            deliveries: vec![
                Self::record(1, State::Handled),
                Self::record(2, State::Queued),
                Self::record(3, State::Processing),
                Self::record(4, State::Failed),
                Self::record(5, State::Ignored),
            ],
            live: true,
        }
    }

    fn task(id: i64, brief: &str, position: QueuePosition) -> Task {
        let source: Source = format!("issue:{id:040x}").parse().expect("source");
        Task {
            id,
            brief: brief.to_owned(),
            context: TaskContext::new(brief.to_owned(), &source, None),
            source,
            position,
        }
    }

    fn record(id: u32, state: State) -> Record {
        let activity = Activity {
            repository: "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc".parse().expect("RID"),
            subject: Subject::Patch(
                "2222222222222222222222222222222222222222"
                    .parse()
                    .expect("patch"),
            ),
            operation: format!("{id:040x}").parse().expect("operation"),
            action: 0,
            author: "did:key:z6MkvUJtYD9dHDJfpevWRT98mzDDpdAtmUjwyDSkyqksUr7C"
                .parse()
                .expect("DID"),
            kind: Kind::Delivered(Delivery::Mentioned(Target::Subject)),
        };
        Record {
            state,
            received: u64::from(id),
            ..Record::new(activity, None)
        }
    }

    fn key(screen: &mut Screen, code: KeyCode) {
        assert!(!screen.key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn render(screen: &mut Screen, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| View::new(screen).draw(frame))
            .expect("draw frame");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn logs(count: u64) -> Vec<LogLine> {
        (1..=count)
            .map(|id| LogLine {
                id,
                text: format!("warning {id}").into(),
            })
            .collect()
    }
}

#[test]
fn live_updates_preserve_selection_and_execution_order() {
    let mut screen = Screen::new(Fixture::inspection());
    Fixture::key(&mut screen, KeyCode::Down);
    assert_eq!(screen.tasks.id.as_deref(), Some("3"));
    let mut update = Fixture::inspection();
    update.tasks.insert(
        1,
        (
            Fixture::task(8, "Urgent incoming task", QueuePosition::Front),
            false,
        ),
    );
    screen.refresh(update);
    assert_eq!(screen.phase(), Phase::Running);
    assert_eq!(screen.tasks.id.as_deref(), Some("3"));
    assert_eq!(screen.tasks.state.selected(), Some(2));
    assert_eq!(screen.task_ids(), ["2", "8", "3", "4"]);
    Fixture::key(&mut screen, KeyCode::Enter);
    assert!(matches!(&screen.detail, Some(Detail::Task(id)) if id == "3"));
    let mut finished = Fixture::inspection();
    finished.tasks.retain(|(task, _)| task.id != 3);
    screen.refresh(finished);
    assert!(Fixture::render(&mut screen, 110, 30).contains("context is no longer retained"));
    Fixture::key(&mut screen, KeyCode::Esc);
    assert!(screen.detail.is_none());
}

#[test]
fn inbox_selection_tracks_operations_as_notifications_change_state() {
    let mut screen = Screen::new(Fixture::inspection());
    screen.focus = Panel::Inbox;
    assert_eq!(
        screen
            .data
            .deliveries
            .first()
            .expect("first delivery")
            .state,
        State::Failed
    );
    Fixture::key(&mut screen, KeyCode::Down);
    let selected = screen.inbox.id.clone();
    let mut update = Fixture::inspection();
    for record in &mut update.deliveries {
        if Some(record.id()) == selected {
            record.state = State::Handled;
        }
    }
    screen.refresh(update);
    assert_eq!(screen.inbox.id, selected);
    Fixture::key(&mut screen, KeyCode::Enter);
    assert!(matches!(&screen.detail, Some(Detail::Delivery(id)) if Some(id) == selected.as_ref()));
    assert!(Fixture::render(&mut screen, 110, 30).contains("Event references"));
}

#[test]
fn logs_keep_the_reading_position_and_count_new_entries() {
    let mut logs = LogView::default();
    logs.page = 3;
    logs.extend(Fixture::logs(10));
    assert_eq!(logs.start(&logs.visible()), 7);
    logs.scroll(-2);
    let before = logs
        .visible()
        .get(logs.start(&logs.visible()))
        .expect("visible line")
        .id;
    logs.extend(vec![LogLine {
        id: 11,
        text: "failed fetch".into(),
    }]);
    assert!(!logs.following);
    assert_eq!(logs.unseen(), 1);
    assert_eq!(
        logs.visible()
            .get(logs.start(&logs.visible()))
            .expect("visible line")
            .id,
        before
    );
    logs.home();
    assert_eq!(logs.start(&logs.visible()), 0);
    logs.toggle_follow();
    assert!(logs.following);
    assert_eq!(logs.unseen(), 0);
    logs.filter = "FAILED".to_owned();
    assert_eq!(logs.visible().len(), 1);
    logs.toggle_follow();
    assert!(!logs.following);
    logs.follow();
}

#[test]
fn tracing_events_are_complete_bounded_and_never_written_to_the_screen() {
    let buffer = LogBuffer::default();
    {
        let mut writer = buffer.make_writer();
        writer
            .write_all("first 😀".as_bytes())
            .expect("first fragment");
        writer
            .write_all(b" part\nsecond\t\x1b[31mline\x1b[0m\x07\x1b[2J\x1b\n")
            .expect("second fragment");
        writer.flush().expect("flush");
        assert!(buffer.since(0).is_empty());
    }
    let lines = buffer.since(0);
    assert_eq!(
        lines.first().expect("first line").text.to_string(),
        "first 😀 part"
    );
    assert_eq!(
        lines.last().expect("last line").text.to_string(),
        "second    line"
    );
    let before = lines.last().expect("last line").id;
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(buffer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || tracing::warn!("captured warning"));
    let captured = buffer.since(before);
    let event = &captured.first().expect("captured event").text;
    assert!(event.to_string().contains(" WARN "));
    assert!(event.to_string().ends_with("captured warning"));
    let mut view = LogView::default();
    view.extend(lines);
    view.filter = "31m".to_owned();
    assert!(
        view.visible().is_empty(),
        "ANSI codes must not enter searches"
    );
    view.filter = "SECOND".to_owned();
    assert_eq!(view.visible().len(), 1);
    for id in 0..2_100 {
        writeln!(buffer.make_writer(), "entry {id}").expect("bounded capture");
    }
    assert_eq!(buffer.since(0).len(), 2_000);
    let mut view = LogView::default();
    view.extend(Fixture::logs(3_000));
    assert_eq!(view.lines.len(), 2_000);
    view.home();
    assert_eq!(
        view.visible().first().expect("oldest retained line").id,
        1_001
    );
}

#[test]
fn log_bindings_scroll_lines_pages_and_resume_following() {
    let mut screen = Screen::new(Fixture::inspection());
    Fixture::key(&mut screen, KeyCode::Char('l'));
    screen.logs.extend(Fixture::logs(30));
    screen.logs.page = 5;
    Fixture::key(&mut screen, KeyCode::Up);
    assert_eq!(screen.logs.start(&screen.logs.visible()), 24);
    Fixture::key(&mut screen, KeyCode::PageUp);
    assert_eq!(screen.logs.start(&screen.logs.visible()), 19);
    Fixture::key(&mut screen, KeyCode::PageDown);
    assert_eq!(screen.logs.start(&screen.logs.visible()), 24);
    Fixture::key(&mut screen, KeyCode::Down);
    assert_eq!(screen.logs.start(&screen.logs.visible()), 25);
    Fixture::key(&mut screen, KeyCode::Home);
    assert_eq!(screen.logs.start(&screen.logs.visible()), 0);
    Fixture::key(&mut screen, KeyCode::End);
    assert!(screen.logs.following);
    Fixture::key(&mut screen, KeyCode::Char(' '));
    assert!(!screen.logs.following);
    Fixture::key(&mut screen, KeyCode::Char(' '));
    assert!(screen.logs.following);
    Fixture::key(&mut screen, KeyCode::Char('x'));
    assert!(screen.logs.following, "unbound keys must not pause logs");
}

#[test]
fn wheel_scrolling_targets_visible_panels_and_preserves_paused_logs() {
    let mut screen = Screen::new(Fixture::inspection());
    screen.logs.extend(Fixture::logs(40));
    let _view = Fixture::render(&mut screen, 120, 32);
    let logs = *screen.areas.get(&Panel::Logs).expect("log area");
    let before = screen.logs.start(&screen.logs.visible());
    let mut wheel = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: logs.x + 1,
        row: logs.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    screen.mouse(wheel);
    assert_eq!(screen.focus, Panel::Logs);
    assert!(!screen.logs.following);
    assert_eq!(screen.logs.start(&screen.logs.visible()), before - 1);
    screen.logs.extend(vec![LogLine {
        id: 41,
        text: "new entry during wheel scrolling".into(),
    }]);
    assert_eq!(screen.logs.start(&screen.logs.visible()), before - 1);
    assert_eq!(screen.logs.unseen(), 1);
    wheel.kind = MouseEventKind::ScrollDown;
    screen.mouse(wheel);
    assert_eq!(screen.logs.start(&screen.logs.visible()), before);
    wheel.kind = MouseEventKind::Down(MouseButton::Left);
    screen.mouse(wheel);
    assert_eq!(screen.logs.start(&screen.logs.visible()), before);

    let tasks = *screen.areas.get(&Panel::Tasks).expect("task area");
    wheel.kind = MouseEventKind::ScrollDown;
    wheel.column = tasks.x + 1;
    wheel.row = tasks.y + 1;
    screen.mouse(wheel);
    assert_eq!(screen.focus, Panel::Tasks);
    assert_eq!(screen.tasks.id.as_deref(), Some("3"));
    let _view = Fixture::render(&mut screen, 76, 20);
    assert_eq!(
        screen.areas.keys().copied().collect::<Vec<_>>(),
        [Panel::Tasks]
    );
    let tasks = *screen.areas.get(&Panel::Tasks).expect("compact task area");
    wheel.column = tasks.x + 1;
    wheel.row = tasks.y + 1;
    screen.mouse(wheel);
    assert_eq!(screen.tasks.id.as_deref(), Some("4"));
    assert_eq!(screen.logs.start(&screen.logs.visible()), before);

    let (task, _) = screen.data.tasks.last_mut().expect("selected task");
    task.context.notes = (0..40).map(|line| format!("Context line {line}")).collect();
    Fixture::key(&mut screen, KeyCode::Enter);
    let _view = Fixture::render(&mut screen, 76, 20);
    let overlay = screen.overlay.expect("detail area");
    wheel.column = overlay.x + 1;
    wheel.row = overlay.y + 1;
    screen.mouse(wheel);
    assert_eq!(screen.detail_scroll, 1);
    wheel.column = 0;
    wheel.row = 0;
    screen.mouse(wheel);
    assert_eq!(screen.detail_scroll, 1, "wheel outside details is ignored");
    Fixture::key(&mut screen, KeyCode::Esc);
    Fixture::key(&mut screen, KeyCode::Char('/'));
    let _view = Fixture::render(&mut screen, 76, 20);
    wheel.column = tasks.x + 1;
    wheel.row = tasks.y + 1;
    wheel.kind = MouseEventKind::ScrollUp;
    screen.mouse(wheel);
    assert_eq!(
        screen.tasks.id.as_deref(),
        Some("4"),
        "filter editing keeps its selection"
    );
}

#[tokio::test]
async fn log_selection_exports_complete_plain_lines_and_survives_arrivals() {
    let buffer = LogBuffer::default();
    {
        let mut writer = buffer.make_writer();
        writeln!(
            writer,
            "\x1b[31mfirst 😀\x1b[0m {}",
            "long line ".repeat(30)
        )
        .expect("long log line");
        writeln!(writer, "second line").expect("second line");
        writeln!(writer, "third line").expect("third line");
    }
    let mut screen = Screen::new(Fixture::inspection());
    screen.logs.extend(buffer.since(0));
    let _view = Fixture::render(&mut screen, 120, 32);
    let area = *screen.areas.get(&Panel::Logs).expect("log area");
    let mut mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 1,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    };
    screen.mouse(mouse);
    mouse.kind = MouseEventKind::Drag(MouseButton::Left);
    mouse.row += 1;
    screen.mouse(mouse);
    mouse.kind = MouseEventKind::Up(MouseButton::Left);
    mouse.column = 0;
    screen.mouse(mouse);
    assert!(!screen.logs.dragging);
    assert!(!screen.logs.following);
    assert_eq!(screen.focus, Panel::Logs);
    let expected = format!("first 😀 {}\nsecond line\n", "long line ".repeat(30));
    assert_eq!(screen.logs.export_text(), expected);
    writeln!(buffer.make_writer(), "arrival during selection").expect("new log");
    screen.logs.extend(buffer.since(screen.logs.latest()));
    assert_eq!(screen.logs.export_text(), expected);

    let mut terminal = Terminal::new(TestBackend::new(120, 32)).expect("terminal");
    terminal
        .draw(|frame| View::new(&mut screen).draw(frame))
        .expect("selection frame");
    let first = terminal
        .backend()
        .buffer()
        .cell((area.x + 1, area.y + 1))
        .expect("log cell");
    assert!(first.modifier.contains(Modifier::REVERSED));
    assert_eq!(first.fg, Color::Red, "selection keeps original log styling");
    Fixture::key(&mut screen, KeyCode::Char('y'));
    assert!(matches!(screen.export.take(), Some(Export::Copy(text)) if text == expected));

    Fixture::key(&mut screen, KeyCode::Char('w'));
    assert!(Fixture::render(&mut screen, 76, 20).contains("Save log"));
    assert!(!screen.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    Fixture::key(&mut screen, KeyCode::Backspace);
    assert!(!screen.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)));
    assert!(screen.saving.as_ref().expect("save prompt").path.is_empty());
    Fixture::key(&mut screen, KeyCode::Enter);
    assert!(
        screen.saving.is_some(),
        "an empty path keeps the prompt open"
    );
    assert!(matches!(&screen.notice, Some(Err(message)) if message == "Enter a file path"));
    let root = tempfile::tempdir().expect("export directory");
    let path = root.path().join("q-logs.txt");
    screen.paste(path.to_str().expect("UTF-8 export path"));
    Fixture::key(&mut screen, KeyCode::Enter);
    let export = screen.export.take().expect("save requested");
    writeln!(buffer.make_writer(), "arrival after save requested").expect("new log");
    screen.logs.extend(buffer.since(screen.logs.latest()));
    let message = export.execute().await.expect("log saved");
    assert!(message.contains(path.to_str().expect("export path")));
    assert_eq!(
        std::fs::read_to_string(&path).expect("plain saved log"),
        expected
    );

    Fixture::key(&mut screen, KeyCode::Char('w'));
    Fixture::key(&mut screen, KeyCode::Esc);
    assert!(screen.saving.is_none());
    Fixture::key(&mut screen, KeyCode::Esc);
    assert!(screen.logs.selection.is_none());
    Fixture::key(&mut screen, KeyCode::Char('/'));
    screen.paste("second");
    Fixture::key(&mut screen, KeyCode::Enter);
    Fixture::key(&mut screen, KeyCode::Char('y'));
    assert!(matches!(screen.export.take(), Some(Export::Copy(text)) if text == "second line\n"));
    screen.logs.filter = "absent".to_owned();
    Fixture::key(&mut screen, KeyCode::Char('w'));
    assert!(screen.saving.is_none());
    Fixture::key(&mut screen, KeyCode::Char('y'));
    assert!(screen.export.is_none());
}

#[tokio::test]
async fn log_saving_preserves_existing_files_and_reports_invalid_destinations() {
    let root = tempfile::tempdir().expect("export directory");
    let existing = root.path().join("existing.txt");
    std::fs::write(&existing, "keep this").expect("existing file");
    for path in [existing.clone(), root.path().join("missing/log.txt")] {
        let result = Export::Save {
            path,
            contents: "new log\n".to_owned(),
        }
        .execute()
        .await;
        assert!(matches!(
            result,
            Err(crate::cli::dashboard::export::Error::File { .. })
        ));
    }
    assert_eq!(
        std::fs::read_to_string(existing).expect("existing file"),
        "keep this"
    );
    assert!(!root.path().join("missing").exists());
    let mut view = LogView::default();
    view.page = 3;
    view.extend(Fixture::logs(10));
    view.select_row(2, false);
    view.select_row(0, true);
    assert_eq!(view.export_text(), "warning 8\nwarning 9\nwarning 10\n");
    view.extend(Fixture::logs(2_010).into_iter().skip(10).collect());
    assert!(
        view.export_text().is_empty(),
        "eviction must not export unrelated lines"
    );
    view.follow();
    assert!(view.selection.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn clipboard_process_receives_the_full_selection_and_reports_failures() {
    let root = tempfile::tempdir().expect("clipboard fixture");
    let path = root.path().join("clipboard.txt");
    let text = "first 😀\nsecond line\n";
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", "cat > \"$1\"", "clipboard-fixture"])
        .arg(&path);
    let copied = Export::copy_to_process(text.to_owned(), command)
        .await
        .expect("copied");
    assert_eq!(copied, "Copied 2 log lines");
    assert_eq!(
        std::fs::read_to_string(path).expect("clipboard contents"),
        text
    );
    let mut command = tokio::process::Command::new("/bin/sh");
    command.args(["-c", "cat >/dev/null; exit 1"]);
    let error = Export::copy_to_process(text.to_owned(), command)
        .await
        .expect_err("copy failure");
    assert!(error.to_string().contains("exited with"));
    let missing = tokio::process::Command::new(root.path().join("absent-copy-program"));
    let error = Export::copy_to_process(text.to_owned(), missing)
        .await
        .expect_err("missing clipboard program");
    assert!(matches!(
        error,
        crate::cli::dashboard::export::Error::Clipboard(source)
            if source.kind() == std::io::ErrorKind::NotFound
    ));
}

#[tokio::test]
async fn failed_log_exports_leave_the_dashboard_running_and_refreshing() {
    use crossterm::event::Event;
    use futures_util::{StreamExt as _, stream};

    use crate::{cli::dashboard::runtime::Dashboard, shutdown::Shutdown};

    let root = tempfile::tempdir().expect("export fixture");
    let mut screen = Screen::new(Fixture::inspection());
    screen.focus = Panel::Logs;
    let logs = LogBuffer::default();
    writeln!(logs.make_writer(), "export this line").expect("log record");
    screen.logs.extend(logs.since(0));
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).expect("terminal");
    let shutdown = Shutdown::default();
    let (updates, mut receiver) = tokio::sync::mpsc::channel(1);
    let (input, keys) = tokio::sync::mpsc::channel(5);
    for event in [
        Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        Event::Paste(root.path().to_str().expect("fixture path").to_owned()),
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    ] {
        input.send(Ok(event)).await.expect("export input");
    }
    let arrival = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let mut data = Fixture::inspection();
        data.tasks.clear();
        updates.send(Ok(data)).await.expect("live refresh");
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(input);
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    let events = stream::unfold(keys, |mut input| async move {
        input.recv().await.map(|event| (event, input))
    })
    .boxed();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        Dashboard::drive(
            &mut terminal,
            &mut screen,
            &mut receiver,
            &logs,
            &shutdown,
            events,
        ),
    )
    .await
    .expect("responsive after export failure");
    assert!(matches!(
        result,
        Err(crate::cli::dashboard::runtime::Error::Terminal(error))
            if error.kind() == std::io::ErrorKind::UnexpectedEof
    ));
    arrival.await.expect("arrivals");
    assert!(
        !shutdown.is_requested(),
        "export failure must not stop the member"
    );
    assert!(screen.data.tasks.is_empty(), "inspection keeps refreshing");
    assert!(matches!(&screen.notice, Some(Err(error)) if error.contains("could not save log")));
    assert!(Fixture::render(&mut screen, 120, 32).contains("could not save log"));
}

#[test]
fn captured_logs_preserve_normal_formatter_styles_and_resets() {
    for ansi in [true, false] {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(ansi)
            .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
                "time".to_owned(),
            ))
            .with_max_level(Level::TRACE)
            .with_writer(logs.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!("worker", member = "maya").entered();
            tracing::error!(target: "runtime", task = 42, "error entry");
            tracing::warn!(target: "runtime", task = 42, "warn entry");
            tracing::info!(target: "runtime", task = 42, "info entry");
            tracing::debug!(target: "runtime", task = 42, "debug entry");
            tracing::trace!(target: "runtime", task = 42, "trace entry");
        });
        let mut screen = Screen::new(Fixture::inspection());
        screen.focus = Panel::Logs;
        screen.logs.extend(logs.since(0));
        let mut terminal = Terminal::new(TestBackend::new(76, 20)).expect("test terminal");
        terminal
            .draw(|frame| View::new(&mut screen).draw(frame))
            .expect("draw formatted logs");
        for (level, message, color) in [
            ("ERROR", "error entry", Color::Red),
            ("WARN", "warn entry", Color::Yellow),
            ("INFO", "info entry", Color::Green),
            ("DEBUG", "debug entry", Color::Blue),
            ("TRACE", "trace entry", Color::Magenta),
        ] {
            let row = terminal
                .backend()
                .buffer()
                .content
                .chunks(76)
                .find(|row| {
                    row.iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .contains(message)
                })
                .expect("rendered log entry");
            let text: String = row.iter().map(|cell| cell.symbol()).collect();
            for (fragment, foreground, modifier) in [
                ("time", Color::Reset, Modifier::DIM),
                (level, color, Modifier::empty()),
                ("worker", Color::Reset, Modifier::BOLD),
                ("runtime", Color::Reset, Modifier::DIM),
                (message, Color::Reset, Modifier::empty()),
                ("task", Color::Reset, Modifier::ITALIC),
                ("=", Color::Reset, Modifier::DIM),
                ("42", Color::Reset, Modifier::empty()),
            ] {
                let offset = text.find(fragment).expect("formatted log fragment");
                let start = text
                    .get(..offset)
                    .expect("fragment boundary")
                    .chars()
                    .count();
                for cell in row.iter().skip(start).take(fragment.len()) {
                    assert_eq!(cell.fg, if ansi { foreground } else { Color::Reset });
                    assert_eq!(
                        cell.modifier,
                        if ansi { modifier } else { Modifier::empty() }
                    );
                }
            }
        }
    }
}

#[test]
fn filtering_and_narrow_layout_preserve_navigation_and_quit() {
    let mut screen = Screen::new(Fixture::inspection());
    assert!(Fixture::render(&mut screen, 76, 20).contains("Tasks (3)"));
    Fixture::key(&mut screen, KeyCode::Char('/'));
    for character in "undo".chars() {
        Fixture::key(&mut screen, KeyCode::Char(character));
    }
    Fixture::key(&mut screen, KeyCode::Enter);
    assert_eq!(screen.task_ids(), ["3"]);
    assert!(Fixture::render(&mut screen, 76, 20).contains("Safe undo"));
    Fixture::key(&mut screen, KeyCode::Esc);
    assert_eq!(screen.task_ids().len(), 3);
    Fixture::key(&mut screen, KeyCode::Tab);
    assert_eq!(screen.focus, Panel::Inbox);
    Fixture::key(&mut screen, KeyCode::BackTab);
    assert_eq!(screen.focus, Panel::Tasks);
    Fixture::key(&mut screen, KeyCode::Char('i'));
    Fixture::key(&mut screen, KeyCode::Char('/'));
    Fixture::key(&mut screen, KeyCode::Char('q'));
    assert!(screen.editing, "q is text while entering a filter");
    Fixture::key(&mut screen, KeyCode::Backspace);
    Fixture::key(&mut screen, KeyCode::Char('!'));
    assert!(Fixture::render(&mut screen, 76, 20).contains("No matching notifications"));
    Fixture::key(&mut screen, KeyCode::Esc);
    Fixture::key(&mut screen, KeyCode::Char('s'));
    Fixture::key(&mut screen, KeyCode::Enter);
    assert!(Fixture::render(&mut screen, 76, 20).contains("Member status"));
    Fixture::key(&mut screen, KeyCode::PageDown);
    Fixture::key(&mut screen, KeyCode::Home);
    Fixture::key(&mut screen, KeyCode::End);
    Fixture::key(&mut screen, KeyCode::Enter);
    Fixture::key(&mut screen, KeyCode::Char('?'));
    assert!(Fixture::render(&mut screen, 76, 20).contains("Keyboard"));
    Fixture::key(&mut screen, KeyCode::Down);
    Fixture::key(&mut screen, KeyCode::PageUp);
    Fixture::key(&mut screen, KeyCode::Char('?'));
    assert!(!screen.help);
    assert!(screen.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    assert!(screen.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
}

#[test]
fn rendering_exposes_failures_stale_data_and_interrupted_activity() {
    let mut screen = Screen::new(Fixture::inspection());
    screen.refresh(Fixture::inspection());
    let wide = Fixture::render(&mut screen, 120, 32);
    for text in [
        "rad-team",
        "running",
        "Status",
        "Tasks · 3",
        "Inbox · 1 waiting",
        "Logs · following",
        "Monotonic task IDs",
        "DID:",
    ] {
        assert!(wide.contains(text), "{text}: {wide}");
    }
    screen.refresh_error = Some("storage busy; retrying".to_owned());
    assert!(Fixture::render(&mut screen, 120, 32).contains("storage busy; retrying"));
    screen.refresh_error = None;
    screen.refreshed = Instant::now()
        .checked_sub(Duration::from_secs(8))
        .expect("recent instant");
    assert!(Fixture::render(&mut screen, 120, 32).contains("Display update delayed"));
    let mut update = Fixture::inspection();
    update.live = false;
    screen.refresh(update);
    assert!(Fixture::render(&mut screen, 120, 32).contains("interrupted"));
    screen.stopping = true;
    assert!(Fixture::render(&mut screen, 120, 32).contains("stopping"));
    for (width, height) in [(1, 1), (15, 4), (50, 8), (100, 24)] {
        let _view = Fixture::render(&mut screen, width, height);
    }
}

#[test]
fn inbox_event_labels_are_readable_for_all_activity_categories() {
    let id = "3333333333333333333333333333333333333333"
        .parse()
        .expect("entry");
    let review = radicle::cob::patch::ReviewId::from(id);
    let revision = radicle::cob::patch::RevisionId::from(
        "4444444444444444444444444444444444444444"
            .parse::<radicle::cob::EntryId>()
            .expect("revision"),
    );
    let comment = Comment::Issue(id);
    let mut record = Fixture::record(1, State::Queued);
    for (kind, label) in [
        (Kind::Assigned, "assignment"),
        (
            Kind::Delivered(Delivery::Replied {
                comment,
                reply_to: id,
            }),
            "reply",
        ),
        (
            Kind::Delivered(Delivery::Mentioned(Target::Subject)),
            "mention",
        ),
    ] {
        record.activity.kind = kind;
        assert_eq!(EventLabel::of(&record), label);
    }
    for (change, label) in [
        (Change::ReviewSubmitted { review, revision }, "review"),
        (Change::RevisionCreated(revision), "revision"),
        (Change::CommentCreated(comment), "comment"),
        (
            Change::CommentReaction {
                comment,
                active: true,
            },
            "reaction",
        ),
        (Change::ReviewCommentsResolved, "resolution"),
        (Change::Unassigned, "unassigned"),
        (Change::LabelsChanged, "metadata"),
        (Change::PatchReady, "lifecycle"),
        (Change::Merged, "merge"),
    ] {
        record.activity.kind = Kind::Delivered(Delivery::Changed(change));
        assert_eq!(EventLabel::of(&record), label);
    }
}

#[test]
fn full_details_preserve_task_relationships_notes_and_delivery_authors() {
    let mut data = Fixture::inspection();
    let (task, _) = data.tasks.first_mut().expect("task");
    task.context.specification = "Delegate specification: keep ID allocation monotonic".to_owned();
    task.context.cancelled = true;
    task.context.notes = vec!["The original review defines the intended behavior".to_owned()];
    for (index, role) in [
        ArtifactRole::Origin,
        ArtifactRole::Implementation,
        ArtifactRole::Review,
        ArtifactRole::Produced,
        ArtifactRole::Supporting,
    ]
    .into_iter()
    .enumerate()
    {
        task.context.artifacts.push(Artifact {
            subject: format!("patch:{:040x}", index + 100),
            role,
            purpose: format!("Relationship {index}"),
        });
    }
    task.context.messages.push(MessageReference {
        subject: "patch:2222222222222222222222222222222222222222".to_owned(),
        entry: "7777777777777777777777777777777777777777".to_owned(),
        revision: Some("8888888888888888888888888888888888888888".to_owned()),
        purpose: "Delegate review".to_owned(),
    });
    let snapshot = data.snapshot.as_mut().expect("snapshot");
    snapshot.sync_error = Some("The seed is temporarily unavailable".to_owned());
    snapshot.failure = Some("The previous session was interrupted".to_owned());
    let mut screen = Screen::new(data);
    screen.refresh_error = Some("Temporary inspection failure".to_owned());
    Fixture::key(&mut screen, KeyCode::Enter);
    let task = Fixture::render(&mut screen, 120, 55);
    for text in [
        "Delegate specification",
        "Cancellation requested",
        "origin",
        "implementation",
        "review",
        "produced",
        "supporting",
        "Delegate review",
        "8888888888888888888888888888888888888888",
        "Notes",
        "original review",
    ] {
        assert!(task.contains(text), "{text}: {task}");
    }
    Fixture::key(&mut screen, KeyCode::Esc);
    Fixture::key(&mut screen, KeyCode::Char('s'));
    Fixture::key(&mut screen, KeyCode::Enter);
    let status = Fixture::render(&mut screen, 120, 55);
    for text in [
        "Last successful fetch",
        "Last activity",
        "Source:",
        "Task: #2",
        "Last progress",
        "Sync error:",
        "Last error:",
        "Display refresh error",
        "Home:",
        "DID:",
    ] {
        assert!(status.contains(text), "{text}: {status}");
    }
    Fixture::key(&mut screen, KeyCode::Esc);
    screen.focus = Panel::Inbox;
    let record = screen.data.deliveries.first_mut().expect("notification");
    record.task = Some(2);
    record.error = Some("The fetch will be retried".to_owned());
    Fixture::key(&mut screen, KeyCode::Enter);
    let notification = Fixture::render(&mut screen, 120, 55);
    for text in [
        "Author: did:key:",
        "Operation:",
        "Received:",
        "Task: #2",
        "Reason: The fetch will be retried",
    ] {
        assert!(notification.contains(text), "{text}: {notification}");
    }
    screen.data.deliveries.clear();
    assert!(Fixture::render(&mut screen, 120, 30).contains("notification is no longer available"));
}

#[test]
fn empty_queues_and_lifecycle_transitions_remain_clear_at_any_size() {
    let mut screen = Screen::new(Fixture::inspection());
    for phase in [
        Phase::Starting,
        Phase::Synchronizing,
        Phase::AwaitingApproval,
        Phase::Initializing,
        Phase::Running,
        Phase::Stopping,
        Phase::Stopped,
        Phase::Failed,
        Phase::Interrupted,
    ] {
        let mut data = Fixture::inspection();
        data.snapshot.as_mut().expect("snapshot").phase = phase;
        data.tasks.clear();
        data.deliveries.clear();
        for role in data.snapshot.as_mut().expect("snapshot").roles.values_mut() {
            role.activity = None;
        }
        screen.refresh(data);
        let view = Fixture::render(&mut screen, 120, 30);
        assert!(view.contains(&phase.to_string()), "{view}");
        assert!(view.contains("No unfinished tasks"));
    }
    let mut empty = Fixture::inspection();
    empty.snapshot = None;
    empty.did = None;
    empty.live = false;
    empty.tasks.clear();
    empty.deliveries.clear();
    screen.refresh(empty);
    assert!(Fixture::render(&mut screen, 120, 30).contains("loading identity"));
    for level in [
        Level::ERROR,
        Level::WARN,
        Level::INFO,
        Level::DEBUG,
        Level::TRACE,
    ] {
        let id = screen.logs.latest() + 1;
        screen.logs.extend(vec![LogLine {
            id,
            text: format!("{level} log entry").into(),
        }]);
    }
    screen.focus = Panel::Logs;
    assert!(Fixture::render(&mut screen, 76, 20).contains("q stop"));
    Fixture::key(&mut screen, KeyCode::Char('/'));
    for character in "missing".chars() {
        Fixture::key(&mut screen, KeyCode::Char(character));
    }
    Fixture::key(&mut screen, KeyCode::Enter);
    assert!(Fixture::render(&mut screen, 76, 20).contains("No matching log entries"));
    Fixture::key(&mut screen, KeyCode::Esc);
    for panel in Panel::ALL {
        screen.focus = panel;
        let _view = Fixture::render(&mut screen, 50, 10);
        Fixture::key(&mut screen, KeyCode::Tab);
        Fixture::key(&mut screen, KeyCode::BackTab);
        assert_eq!(screen.focus, panel);
    }
    Fixture::key(&mut screen, KeyCode::Char('?'));
    for (width, height) in [(1, 1), (15, 4), (50, 8)] {
        let _view = Fixture::render(&mut screen, width, height);
    }
}

#[tokio::test]
async fn dashboard_keeps_updating_during_paused_logs_and_shutdown() {
    use crossterm::event::{Event, KeyEventKind};
    use futures_util::{StreamExt as _, stream};

    use crate::{cli::dashboard::runtime::Dashboard, shutdown::Shutdown};

    let mut screen = Screen::new(Fixture::inspection());
    Fixture::key(&mut screen, KeyCode::Down);
    let logs = LogBuffer::default();
    for id in 1..=20 {
        writeln!(logs.make_writer(), "initial {id}").expect("seed logs");
    }
    screen.logs.extend(logs.since(0));
    let mut terminal = Terminal::new(TestBackend::new(110, 30)).expect("test terminal");
    terminal
        .draw(|frame| View::new(&mut screen).draw(frame))
        .expect("initial frame");
    let log_area = *screen.areas.get(&Panel::Logs).expect("log area");
    let shutdown = Shutdown::default();
    let (updates, mut receiver) = tokio::sync::mpsc::channel(2);
    updates
        .send(Err(crate::status::Error::Io(std::io::Error::other(
            "temporary read failure",
        ))))
        .await
        .expect("error observation");
    let (keys, input) = tokio::sync::mpsc::channel(4);
    keys.send(Ok(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ))))
    .await
    .expect("key release");
    keys.send(Ok(Event::Key(KeyEvent::new(
        KeyCode::Char('l'),
        KeyModifiers::NONE,
    ))))
    .await
    .expect("focus logs");
    keys.send(Ok(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: log_area.x + 1,
        row: log_area.y + 1,
        modifiers: KeyModifiers::NONE,
    })))
    .await
    .expect("wheel scroll logs");
    let extra_logs = logs.clone();
    let arrival = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let mut next = Fixture::inspection();
        next.tasks.insert(
            1,
            (
                Fixture::task(8, "Incoming assignment", QueuePosition::Front),
                false,
            ),
        );
        updates.send(Ok(next)).await.expect("live update");
        writeln!(extra_logs.make_writer(), "new log while reading history").expect("arrival log");
        tokio::time::sleep(Duration::from_millis(120)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))))
        .await
        .expect("stop key");
        tokio::time::sleep(Duration::from_millis(120)).await;
        drop(keys);
        tokio::time::sleep(Duration::from_millis(120)).await;
    });
    let events = stream::unfold(input, |mut input| async move {
        input.recv().await.map(|event| (event, input))
    })
    .boxed();
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        Dashboard::drive(
            &mut terminal,
            &mut screen,
            &mut receiver,
            &logs,
            &shutdown,
            events,
        ),
    )
    .await
    .expect("responsive UI");
    assert!(
        matches!(outcome, Err(crate::cli::dashboard::runtime::Error::Terminal(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
    );
    arrival.await.expect("test arrivals");
    assert!(shutdown.is_requested());
    assert!(screen.stopping);
    assert!(
        screen.refresh_error.is_none(),
        "a later successful observation clears the error"
    );
    assert_eq!(screen.tasks.id.as_deref(), Some("3"));
    assert_eq!(screen.task_ids(), ["2", "8", "3", "4"]);
    assert!(!screen.logs.following);
    assert_eq!(screen.logs.unseen(), 1);
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(rendered.contains("stopping"));
    assert!(rendered.contains("Paused · 1 new entry"));
}

#[tokio::test]
async fn dashboard_reports_input_failure_and_a_stopped_inspection_worker() {
    use futures_util::stream;

    use crate::{
        cli::dashboard::runtime::{Dashboard, Error},
        shutdown::Shutdown,
    };

    let mut screen = Screen::new(Fixture::inspection());
    let mut terminal = Terminal::new(TestBackend::new(110, 30)).expect("test terminal");
    let logs = LogBuffer::default();
    let shutdown = Shutdown::default();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let outcome = Dashboard::drive(
        &mut terminal,
        &mut screen,
        &mut receiver,
        &logs,
        &shutdown,
        stream::iter([Err(std::io::Error::other("broken input"))]),
    )
    .await;
    assert!(matches!(outcome, Err(Error::Terminal(error)) if error.to_string() == "broken input"));
    drop(sender);
    let outcome = Dashboard::drive(
        &mut terminal,
        &mut screen,
        &mut receiver,
        &logs,
        &shutdown,
        stream::pending(),
    )
    .await;
    assert!(matches!(outcome, Err(Error::MonitorStopped)));
}
