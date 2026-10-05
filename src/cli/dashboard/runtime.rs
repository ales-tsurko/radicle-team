//! Keeps presentation responsive while the member runs and inspection waits on storage.

use std::{
    io::{self, Write as _},
    path::PathBuf,
    time::Duration,
};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, EventStream,
    KeyEventKind,
};
use futures_util::{FutureExt as _, Stream, StreamExt as _, future::BoxFuture};
use ratatui::backend::Backend;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    Application,
    cli::dashboard::{LogBuffer, export, model::Screen, view::View},
    shutdown::Shutdown,
    status::Inspection,
};

pub(crate) struct Dashboard;

impl Dashboard {
    pub(crate) async fn run(
        application: Application,
        shutdown: Shutdown,
        logs: LogBuffer,
    ) -> Result<(), Error> {
        let data = Inspection::read(application.home().map(PathBuf::from)).await?;
        let mut monitor = Monitor::start(data.home.path().into());
        let mut screen = Screen::new(data);
        let mut terminal = Terminal::start()?;
        screen.logs.extend(logs.since(0));
        terminal
            .0
            .draw(|frame| View::new(&mut screen).draw(frame))?;
        let running = application.run();
        tokio::pin!(running);
        tokio::select! {
            result = &mut running => {
                if shutdown.is_requested() {
                    Ok(())
                } else {
                    result.map_err(|error| Error::Application(Box::new(error)))
                }
            }
            result = Self::drive(&mut terminal.0, &mut screen, &mut monitor.updates, &logs, &shutdown, EventStream::new()) => {
                let _handled = shutdown.request();
                if let Err(error) = running.await {
                    tracing::warn!(%error, "member stopped after dashboard failure");
                }
                result
            }
        }
    }

    pub(super) async fn drive<B: Backend>(
        terminal: &mut ratatui::Terminal<B>,
        screen: &mut Screen,
        updates: &mut mpsc::Receiver<Result<Inspection, crate::status::Error>>,
        logs: &LogBuffer,
        shutdown: &Shutdown,
        mut events: impl Stream<Item = io::Result<Event>> + Unpin,
    ) -> Result<(), Error>
    where
        B::Error: Send + Sync + 'static,
    {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut exporting: Option<BoxFuture<'static, Result<String, export::Error>>> = None;
        loop {
            tokio::select! {
                event = events.next() => {
                    let event = event.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed"))??;
                    match event {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            if screen.key(key) && !screen.stopping {
                                screen.stopping = true;
                                let _handled = shutdown.request();
                            }
                        }
                        Event::Paste(text) => screen.paste(&text),
                        _ => {}
                    }
                }
                update = updates.recv() => {
                    match update {
                        Some(Ok(data)) => screen.refresh(data),
                        Some(Err(error)) => screen.refresh_error = Some(error.to_string()),
                        None => return Err(Error::MonitorStopped),
                    }
                }
                _ = tick.tick() => {}
                result = async {
                    match &mut exporting {
                        Some(export) => export.await,
                        None => std::future::pending().await,
                    }
                } => {
                    screen.notice = Some(result.map_err(|error| error.to_string()));
                    exporting = None;
                }
            }
            if let Some(export) = screen.export.take() {
                if exporting.is_some() {
                    screen.notice = Some(Err("An export is already in progress".to_owned()));
                } else {
                    let progress = match &export {
                        export::Export::Copy { .. } => "Copying…",
                        export::Export::Save { .. } => "Saving log…",
                    };
                    screen.notice = Some(Ok(progress.to_owned()));
                    exporting = Some(export.execute().boxed());
                }
            }
            screen.stopping |= shutdown.is_requested();
            screen.logs.extend(logs.since(screen.logs.latest()));
            terminal
                .draw(|frame| View::new(screen).draw(frame))
                .map_err(io::Error::other)?;
        }
    }
}

pub(crate) struct Terminal(ratatui::DefaultTerminal);

impl Terminal {
    fn start() -> io::Result<Self> {
        match ratatui::try_init() {
            Ok(terminal) => {
                let terminal = Self(terminal);
                crossterm::execute!(io::stdout(), DisableMouseCapture, EnableBracketedPaste)?;
                Self::alternate_scroll(true)?;
                let hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    Self::restore();
                    hook(info);
                }));
                Ok(terminal)
            }
            Err(error) => {
                Self::restore();
                Err(error)
            }
        }
    }

    pub(crate) fn restore() {
        let scrolling = Self::alternate_scroll(false);
        let cursor = crossterm::execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::cursor::Show
        );
        let screen = ratatui::try_restore();
        for result in [scrolling, cursor, screen] {
            if let Err(error) = result {
                eprintln!("Error: failed to restore terminal: {error}");
            }
        }
    }

    fn alternate_scroll(enabled: bool) -> io::Result<()> {
        // With native mouse selection, supporting terminals send wheel motion as arrow keys.
        // Save and restore the terminal's previous private mode instead of leaving it changed.
        let mut output = io::stdout().lock();
        output.write_all(if enabled {
            b"\x1b[?1007s\x1b[?1007h"
        } else {
            b"\x1b[?1007l\x1b[?1007r"
        })?;
        output.flush()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        Self::restore();
    }
}

struct Monitor {
    updates: mpsc::Receiver<Result<Inspection, crate::status::Error>>,
    worker: JoinHandle<()>,
}

impl Monitor {
    fn start(home: PathBuf) -> Self {
        let (sender, updates) = mpsc::channel(1);
        let worker = tokio::spawn(async move {
            loop {
                let data = Inspection::read(Some(home.clone())).await;
                if sender.send(data).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Self { updates, worker }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Debug, thiserror::Error)]
/// Failure starting or maintaining the live dashboard.
pub enum Error {
    /// Reading the initial member snapshot failed.
    #[error(transparent)]
    Inspection(#[from] crate::status::Error),
    /// The terminal could not be initialized, read, drawn, or restored.
    #[error("terminal dashboard failed: {0}")]
    Terminal(#[from] io::Error),
    /// The running member failed.
    #[error(transparent)]
    Application(Box<crate::Error>),
    /// The presentation's inspection worker stopped unexpectedly.
    #[error("dashboard inspection worker stopped")]
    MonitorStopped,
}
