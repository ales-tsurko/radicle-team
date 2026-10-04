//! Bounded tracing capture and stable scrolling over retained log lines.

use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{Arc, Mutex, PoisonError},
};

use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;

const CAPACITY: usize = 2_000;
const EVENT_BYTES: usize = 16_384;

/// A bounded tracing writer for the live terminal dashboard.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Buffer>>);

impl LogBuffer {
    pub(super) fn since(&self, id: u64) -> Vec<LogLine> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lines
            .iter()
            .filter(|line| line.id > id)
            .cloned()
            .collect()
    }

    fn append(&self, bytes: &[u8], level: Level) {
        let text = String::from_utf8_lossy(bytes);
        let mut buffer = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for line in text.lines().filter(|line| !line.is_empty()) {
            buffer.sequence = buffer.sequence.saturating_add(1);
            let id = buffer.sequence;
            buffer.lines.push_back(LogLine {
                id,
                level,
                text: line
                    .replace('\t', "    ")
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect(),
            });
            if buffer.lines.len() > CAPACITY {
                buffer.lines.pop_front();
            }
        }
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            buffer: self.clone(),
            level: Level::INFO,
            bytes: Vec::new(),
        }
    }

    fn make_writer_for(&'a self, metadata: &Metadata<'_>) -> Self::Writer {
        let mut writer = self.make_writer();
        writer.level = *metadata.level();
        writer
    }
}

/// Collects one formatted event before publishing its complete lines.
#[derive(Debug)]
pub struct LogWriter {
    buffer: LogBuffer,
    level: Level,
    bytes: Vec<u8>,
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend(
            bytes
                .iter()
                .take(EVENT_BYTES.saturating_sub(self.bytes.len())),
        );
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        self.buffer.append(&self.bytes, self.level);
    }
}

#[derive(Debug, Default)]
struct Buffer {
    sequence: u64,
    lines: VecDeque<LogLine>,
}

#[derive(Debug, Clone)]
pub(super) struct LogLine {
    pub(super) id: u64,
    pub(super) level: Level,
    pub(super) text: String,
}

#[derive(Debug)]
pub(super) struct LogView {
    pub(super) lines: VecDeque<LogLine>,
    pub(super) following: bool,
    pub(super) filter: String,
    pub(super) page: usize,
    anchor: Option<u64>,
    paused_at: u64,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            lines: VecDeque::new(),
            following: true,
            filter: String::new(),
            page: 1,
            anchor: None,
            paused_at: 0,
        }
    }
}

impl LogView {
    pub(super) fn extend(&mut self, lines: Vec<LogLine>) {
        self.lines.extend(lines);
        let excess = self.lines.len().saturating_sub(CAPACITY);
        self.lines.drain(..excess);
    }

    pub(super) fn latest(&self) -> u64 {
        self.lines.back().map_or(0, |line| line.id)
    }

    pub(super) fn unseen(&self) -> u64 {
        if self.following {
            0
        } else {
            self.latest().saturating_sub(self.paused_at)
        }
    }

    pub(super) fn visible(&self) -> Vec<&LogLine> {
        let query = self.filter.to_lowercase();
        self.lines
            .iter()
            .filter(|line| line.text.to_lowercase().contains(&query))
            .collect()
    }

    pub(super) fn start(&self, visible: &[&LogLine]) -> usize {
        let last = visible.len().saturating_sub(self.page);
        if self.following {
            return last;
        }
        self.anchor
            .and_then(|id| visible.iter().position(|line| line.id >= id))
            .unwrap_or(0)
            .min(last)
    }

    pub(super) fn scroll(&mut self, amount: isize) {
        let visible = self.visible();
        let start = self
            .start(&visible)
            .saturating_add_signed(amount)
            .min(visible.len().saturating_sub(self.page));
        let anchor = visible.get(start).map(|line| line.id);
        if self.following {
            self.paused_at = self.latest();
        }
        self.following = false;
        self.anchor = anchor;
    }

    pub(super) fn home(&mut self) {
        self.scroll(isize::MIN);
    }

    pub(super) fn follow(&mut self) {
        self.following = true;
        self.anchor = None;
    }

    pub(super) fn toggle_follow(&mut self) {
        if self.following {
            self.scroll(0);
        } else {
            self.follow();
        }
    }
}
