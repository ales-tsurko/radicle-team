//! Direct selection over rendered text, retaining complete values behind shortened IDs.

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Modifier,
};

#[derive(Debug, Default)]
pub(super) struct TextSelection {
    frame: Buffer,
    bindings: Vec<Binding>,
    references: Vec<Reference>,
    selected: Option<Selected>,
    pub(super) dragging: bool,
}

impl TextSelection {
    pub(super) fn active(&self) -> bool {
        self.selected.is_some()
    }

    pub(super) fn clear(&mut self) {
        self.selected = None;
        self.dragging = false;
    }

    pub(super) fn begin_frame(&mut self) {
        self.bindings.clear();
    }

    pub(super) fn bind(&mut self, area: Rect, displayed: String, value: String) {
        self.bindings.push(Binding {
            area,
            displayed,
            value,
        });
    }

    pub(super) fn bind_text(&mut self, area: Rect, text: &str) {
        for word in text.split(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, ':' | '-' | '_' | '#')
        }) {
            let word = word.trim_end_matches(':');
            if word.len() >= 20
                || word.starts_with("chat:")
                || word.starts_with("ses_")
                || word
                    .strip_prefix('#')
                    .is_some_and(|id| id.parse::<u64>().is_ok())
            {
                let value = word
                    .strip_prefix('#')
                    .or_else(|| {
                        word.split_once(':').and_then(|(kind, id)| {
                            matches!(kind, "issue" | "patch" | "chat").then_some(id)
                        })
                    })
                    .unwrap_or(word);
                self.bind(area, word.to_owned(), value.to_owned());
                if value != word && !word.starts_with('#') {
                    self.bind(area, value.to_owned(), value.to_owned());
                }
            }
        }
    }

    pub(super) fn render(&mut self, buffer: &mut Buffer) {
        if self.frame.area != buffer.area {
            self.clear();
        }
        let mut references: Vec<_> = self
            .bindings
            .iter()
            .flat_map(|binding| binding.locate(buffer))
            .collect();
        if let Some(selected) = &self.selected {
            references.retain(|reference| {
                !selected
                    .area
                    .contains(selected.frame.pos_of(reference.start).into())
            });
            references.extend(selected.references.iter().cloned());
            for y in selected.area.top()..selected.area.bottom() {
                for x in selected.area.left()..selected.area.right() {
                    buffer[(x, y)] = selected.frame[(x, y)].clone();
                }
            }
        }
        self.frame = buffer.clone();
        self.references = references;
        if let Some(selected) = &self.selected {
            for index in selected.start..=selected.end {
                let position = Position::from(buffer.pos_of(index));
                if selected.area.contains(position)
                    && let Some(cell) = buffer.cell_mut(position)
                {
                    cell.set_style(
                        cell.style()
                            .add_modifier(Modifier::REVERSED | Modifier::UNDERLINED),
                    );
                }
            }
        }
    }

    pub(super) fn start(&mut self, area: Rect, position: Position) {
        let area = area.intersection(self.frame.area);
        if !area.contains(position) {
            return;
        }
        let anchor = self.frame.index_of(position.x, position.y);
        let reference = self
            .references
            .iter()
            .filter(|reference| (reference.start..=reference.end).contains(&anchor))
            .min_by_key(|reference| {
                (
                    reference.end - reference.start,
                    reference.value.ends_with('\n'),
                )
            });
        let (start, end) = reference.map_or_else(
            || self.word(area, position),
            |reference| (reference.start, reference.end),
        );
        self.selected = Some(Selected {
            area,
            frame: self.frame.clone(),
            references: self
                .references
                .iter()
                .filter(|reference| area.contains(self.frame.pos_of(reference.start).into()))
                .cloned()
                .collect(),
            anchor,
            start,
            end,
        });
        self.dragging = true;
    }

    fn word(&self, area: Rect, position: Position) -> (usize, usize) {
        let mut left = position.x;
        let mut right = position.x;
        while left > area.left()
            && !self.frame[(left - 1, position.y)]
                .symbol()
                .chars()
                .all(char::is_whitespace)
        {
            left -= 1;
        }
        while right + 1 < area.right()
            && !self.frame[(right + 1, position.y)]
                .symbol()
                .chars()
                .all(char::is_whitespace)
        {
            right += 1;
        }
        (
            self.frame.index_of(left, position.y),
            self.frame.index_of(right, position.y),
        )
    }

    pub(super) fn extend(&mut self, position: Position) {
        let Some(selected) = &mut self.selected else {
            return;
        };
        let x = position.x.clamp(
            selected.area.left(),
            selected.area.right().saturating_sub(1),
        );
        let y = position.y.clamp(
            selected.area.top(),
            selected.area.bottom().saturating_sub(1),
        );
        let mut cursor = selected.frame.index_of(x, y);
        if selected.frame.pos_of(selected.anchor).1 != y
            && let Some(line) = selected.references.iter().find(|reference| {
                reference.value.ends_with('\n')
                    && (reference.start..=reference.end).contains(&cursor)
            })
        {
            cursor = if cursor >= selected.anchor {
                line.end
            } else {
                line.start
            };
        }
        selected.start = selected.anchor.min(cursor);
        selected.end = selected.anchor.max(cursor);
    }

    pub(super) fn contents(&self) -> String {
        self.selected
            .as_ref()
            .map_or(String::new(), Selected::contents)
    }
}

#[derive(Debug)]
struct Selected {
    area: Rect,
    frame: Buffer,
    references: Vec<Reference>,
    anchor: usize,
    start: usize,
    end: usize,
}

impl Selected {
    fn contents(&self) -> String {
        if let Some(reference) = self
            .references
            .iter()
            .filter(|reference| reference.start == self.start && reference.end == self.end)
            .min_by_key(|reference| reference.value.ends_with('\n'))
        {
            return reference.value.clone();
        }
        let mut text = String::new();
        let mut skip = 0;
        let mut previous = false;
        let mut newline = false;
        for y in self.area.top()..self.area.bottom() {
            let mut line = String::new();
            let mut included = false;
            for x in self.area.left()..self.area.right() {
                let index = self.frame.index_of(x, y);
                if index < skip || !(self.start..=self.end).contains(&index) {
                    continue;
                }
                included = true;
                if let Some(reference) = self
                    .references
                    .iter()
                    .filter(|reference| reference.start == index && reference.end <= self.end)
                    .max_by_key(|reference| reference.end)
                {
                    line.push_str(&reference.value);
                    skip = reference.end + 1;
                } else {
                    let symbol = self.frame[(x, y)].symbol();
                    line.push_str(symbol);
                    skip = index + ratatui::text::Span::raw(symbol).width();
                }
            }
            let line = line.trim_end_matches(' ');
            if included {
                if previous && !newline {
                    text.push('\n');
                }
                text.push_str(line);
                previous = true;
                newline = line.ends_with('\n');
            }
        }
        text
    }
}

#[derive(Debug)]
struct Binding {
    area: Rect,
    displayed: String,
    value: String,
}

impl Binding {
    fn locate(&self, buffer: &Buffer) -> Vec<Reference> {
        let area = self.area.intersection(buffer.area);
        let mut characters = Vec::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let index = buffer.index_of(x, y);
                for character in buffer[(x, y)]
                    .symbol()
                    .chars()
                    .filter(|character| !character.is_whitespace())
                {
                    characters.push((character, index));
                }
            }
        }
        let needle: Vec<_> = self
            .displayed
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let mut references = Vec::new();
        for (offset, (_, start)) in characters.iter().enumerate() {
            let count = characters
                .iter()
                .skip(offset)
                .zip(&needle)
                .take_while(|((actual, _), expected)| actual == *expected)
                .count();
            if count == 0 {
                continue;
            }
            let Some((_, last)) = characters.get(offset + count - 1) else {
                continue;
            };
            let clipped = count >= 4
                && (offset + count == characters.len()
                    || buffer.pos_of(*last).0 + 1 == area.right());
            let boundary = characters
                .get(offset + count)
                .is_none_or(|(character, index)| {
                    *index > last + 1 || !character.is_ascii_alphanumeric()
                });
            if (count == needle.len() && boundary) || (count < needle.len() && clipped) {
                references.push(Reference {
                    start: *start,
                    end: *last,
                    value: self.value.clone(),
                });
            }
        }
        references
    }
}

#[derive(Debug, Clone)]
struct Reference {
    start: usize,
    end: usize,
    value: String,
}
