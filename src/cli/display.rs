//! Shared terminal styling and relative timestamps for inspection commands.

use std::io::IsTerminal as _;

use console::Style;

use crate::status::Snapshot;

pub(super) struct Display;

impl Display {
    pub(super) fn style() -> Style {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Style::new().force_styling(
            std::io::stdout().is_terminal() && console::colors_enabled() && !no_color,
        )
    }

    pub(super) fn age(at: u64) -> String {
        let seconds = Snapshot::now().saturating_sub(at);
        match seconds {
            0..60 => format!("{seconds}s ago"),
            60..3600 => format!("{}m {}s ago", seconds / 60, seconds % 60),
            _ => format!("{}h {}m ago", seconds / 3600, (seconds % 3600) / 60),
        }
    }
}
