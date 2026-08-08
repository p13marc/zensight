//! Reusable responsive data-table for specialized views (#244).
//!
//! Both the netring and netlink views hand-roll tables as `row![cell(..), ..]`
//! loops with hardcoded `text(s).size(12)` cells, ~150px fixed column widths, and
//! silent `.take(200)` cutoffs — which overflow the viewport and hide truncation.
//! `DataTable` replaces that pattern with:
//!
//! - typed columns (label + width + a `&T -> Element` cell renderer),
//! - header **sort** (click a sortable column to toggle asc/desc),
//! - an inline **filter** box (substring match over a per-row searchable string),
//! - and an explicit **"showing N of M"** count, instead of a silent cap.
//!
//! The filter box and the count sit **above** the header; only the load-more
//! button sits below. The control that shortens a long table must not be at the
//! far end of it, and the count belongs next to the box whose effect it reports.
//!
//! Sort/filter/limit live in a small [`TableState`] the caller stores per table;
//! the three interactions are wired via caller-supplied message closures so one
//! component serves every table without bespoke handlers.

use iced::widget::{Column as Col, Row, button, column, container, text, text_input};
use iced::{Alignment, Element, Length};

use crate::view::theme;
use crate::view::tokens::{font, space};

/// Default number of rows shown before the "N of M" footer truncates.
pub const DEFAULT_LIMIT: usize = 200;
/// How many more rows each "Show more" click reveals.
pub const LOAD_MORE_STEP: usize = 200;

/// Per-table interaction state (sort column + direction, filter text, row cap).
/// Stored by the caller (e.g. in a `*DetailState`) and mutated by the sort/
/// filter/load-more message handlers.
#[derive(Debug, Clone)]
pub struct TableState {
    /// Index of the column currently sorted on, if any.
    pub sort_col: Option<usize>,
    /// Sort direction (true = ascending).
    pub ascending: bool,
    /// Case-insensitive substring filter (empty = no filter).
    pub filter: String,
    /// Maximum rows rendered (the "N" in "N of M").
    pub limit: usize,
}

impl Default for TableState {
    fn default() -> Self {
        Self {
            sort_col: None,
            ascending: true,
            filter: String::new(),
            limit: DEFAULT_LIMIT,
        }
    }
}

impl TableState {
    /// Click a column header: sort on it ascending, or flip direction if already
    /// the active sort column.
    pub fn toggle_sort(&mut self, col: usize) {
        if self.sort_col == Some(col) {
            self.ascending = !self.ascending;
        } else {
            self.sort_col = Some(col);
            self.ascending = true;
        }
    }

    /// Set the filter text (resets the visible cap so matches aren't hidden).
    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.limit = DEFAULT_LIMIT;
    }

    /// Reveal another page of rows.
    pub fn load_more(&mut self) {
        self.limit = self.limit.saturating_add(LOAD_MORE_STEP);
    }
}

/// A sortable key extracted from a row. Numbers sort numerically (NaN last),
/// text case-insensitively.
#[derive(Debug, Clone)]
pub enum SortKey {
    Num(f64),
    Text(String),
}

impl SortKey {
    fn cmp(&self, other: &SortKey) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.total_cmp(b),
            (SortKey::Text(a), SortKey::Text(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
            // Mixed types shouldn't happen within a column; keep it total.
            (SortKey::Num(_), SortKey::Text(_)) => Ordering::Less,
            (SortKey::Text(_), SortKey::Num(_)) => Ordering::Greater,
        }
    }
}

/// One column of a [`DataTable`]: a header label, a width, a cell renderer, and
/// an optional sort-key extractor (columns without one aren't clickable).
/// Boxed cell renderer: a row reference to its rendered [`Element`].
type CellFn<'a, T, Message> = Box<dyn Fn(&'a T) -> Element<'a, Message> + 'a>;
/// Boxed sort-key extractor for a sortable column.
type SortFn<'a, T> = Box<dyn Fn(&T) -> SortKey + 'a>;

pub struct Column<'a, T, Message> {
    label: String,
    width: Length,
    cell: CellFn<'a, T, Message>,
    sort: Option<SortFn<'a, T>>,
}

impl<'a, T, Message> Column<'a, T, Message> {
    /// A fixed-width column.
    pub fn fixed(
        label: impl Into<String>,
        width: f32,
        cell: impl Fn(&'a T) -> Element<'a, Message> + 'a,
    ) -> Self {
        Self {
            label: label.into(),
            width: Length::Fixed(width),
            cell: Box::new(cell),
            sort: None,
        }
    }

    /// A responsive column taking `portion` of the leftover width (fixes the
    /// viewport overflow the old fixed-px tables suffered).
    pub fn fill(
        label: impl Into<String>,
        portion: u16,
        cell: impl Fn(&'a T) -> Element<'a, Message> + 'a,
    ) -> Self {
        Self {
            label: label.into(),
            width: Length::FillPortion(portion),
            cell: Box::new(cell),
            sort: None,
        }
    }

    /// Make the column sortable via `key`.
    pub fn sortable(mut self, key: impl Fn(&T) -> SortKey + 'a) -> Self {
        self.sort = Some(Box::new(key));
        self
    }
}

/// A responsive, sortable, filterable table. Build with columns, optionally wire
/// `searchable`/`on_sort`/`on_filter`/`on_more`, then call [`DataTable::view`]
/// with the rows and their [`TableState`].
/// Boxed per-row searchable-string extractor for the filter box.
type SearchFn<'a, T> = Box<dyn Fn(&T) -> String + 'a>;
/// Boxed message builder for a table interaction (sort/filter) taking one arg.
type ActionFn<'a, A, Message> = Box<dyn Fn(A) -> Message + 'a>;

/// Boxed row predicate for caller-owned filtering (chips, toggles, …).
type RetainFn<'a, T> = Box<dyn Fn(&T) -> bool + 'a>;

pub struct DataTable<'a, T, Message> {
    columns: Vec<Column<'a, T, Message>>,
    searchable: Option<SearchFn<'a, T>>,
    retain: Option<RetainFn<'a, T>>,
    on_sort: Option<ActionFn<'a, usize, Message>>,
    on_filter: Option<ActionFn<'a, String, Message>>,
    on_more: Option<Message>,
    noun: String,
}

impl<'a, T, Message: Clone + 'a> DataTable<'a, T, Message> {
    pub fn new(columns: Vec<Column<'a, T, Message>>) -> Self {
        Self {
            columns,
            searchable: None,
            retain: None,
            on_sort: None,
            on_filter: None,
            on_more: None,
            noun: "rows".to_string(),
        }
    }

    /// Keep only rows matching `f`, applied *before* the filter box and counted
    /// as the "of M" denominator.
    ///
    /// This is for selectors the caller owns — state chips, type toggles — which
    /// compose with the filter box rather than replacing it: chips narrow *what
    /// the table is about*, the box searches *within that*. Callers used to
    /// pre-filter into a `Vec<&T>` instead, which cannot be handed to
    /// [`DataTable::view`] because the borrow dies with the temporary.
    pub fn retain(mut self, f: impl Fn(&T) -> bool + 'a) -> Self {
        self.retain = Some(Box::new(f));
        self
    }

    /// Provide the per-row string the filter box matches against.
    pub fn searchable(mut self, f: impl Fn(&T) -> String + 'a) -> Self {
        self.searchable = Some(Box::new(f));
        self
    }

    /// Wire sortable headers (click column index -> message).
    pub fn on_sort(mut self, f: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_sort = Some(Box::new(f));
        self
    }

    /// Wire the filter box (text -> message).
    pub fn on_filter(mut self, f: impl Fn(String) -> Message + 'a) -> Self {
        self.on_filter = Some(Box::new(f));
        self
    }

    /// Wire the load-more affordance in the footer.
    pub fn on_more(mut self, msg: Message) -> Self {
        self.on_more = Some(msg);
        self
    }

    /// The noun used in the footer ("showing 20 of 340 flows").
    pub fn noun(mut self, noun: impl Into<String>) -> Self {
        self.noun = noun.into();
        self
    }

    pub fn view(self, rows: &'a [T], st: &TableState) -> Element<'a, Message> {
        // 1. Filter: the caller's row predicate first, then the filter box
        //    within what it left.
        let needle = st.filter.trim().to_lowercase();
        let kept = rows
            .iter()
            .filter(|r| self.retain.as_ref().is_none_or(|keep| keep(r)));
        let mut view_rows: Vec<&'a T> = if needle.is_empty() {
            kept.collect()
        } else if let Some(search) = &self.searchable {
            kept.filter(|r| search(r).to_lowercase().contains(&needle))
                .collect()
        } else {
            kept.collect()
        };
        let total = view_rows.len();

        // 2. Sort.
        if let Some(col) = st.sort_col
            && let Some(key) = self.columns.get(col).and_then(|c| c.sort.as_ref())
        {
            view_rows.sort_by(|a, b| key(a).cmp(&key(b)));
            if !st.ascending {
                view_rows.reverse();
            }
        }

        // 3. Header.
        let mut header = Row::new().spacing(space::SM).align_y(Alignment::Center);
        for (i, c) in self.columns.iter().enumerate() {
            let active = st.sort_col == Some(i);
            let arrow = if active {
                if st.ascending { " ▲" } else { " ▼" }
            } else {
                ""
            };
            let label = format!("{}{arrow}", c.label);
            let head: Element<'a, Message> = if c.sort.is_some() {
                if let Some(on_sort) = &self.on_sort {
                    button(text(label).size(font::CAPTION))
                        .padding(0)
                        .style(button::text)
                        .on_press(on_sort(i))
                        .into()
                } else {
                    text(label).size(font::CAPTION).into()
                }
            } else {
                text(label).size(font::CAPTION).into()
            };
            header = header.push(container(head).width(c.width));
        }

        // 4. Body (capped at the limit).
        let shown = total.min(st.limit);
        let mut body = Col::new().spacing(space::XS);
        for r in view_rows.iter().take(st.limit) {
            let mut line = Row::new().spacing(space::SM).align_y(Alignment::Center);
            for c in &self.columns {
                line = line.push(container((c.cell)(r)).width(c.width));
            }
            body = body.push(line);
        }

        // 5. Controls, ABOVE the table: filter box + "showing N of M".
        //
        // A filter you have to scroll past the whole table to reach is
        // backwards — on a long table the control that shortens it is the one
        // furthest away. The row count sits with it so typing shows its effect
        // without looking anywhere else.
        let mut controls = Row::new().spacing(space::MD).align_y(Alignment::Center);
        if let Some(on_filter) = self.on_filter {
            controls = controls.push(
                text_input("filter…", &st.filter)
                    .on_input(on_filter)
                    .size(font::CAPTION)
                    .width(Length::Fixed(160.0)),
            );
        }
        controls = controls.push(
            text(format!("showing {shown} of {total} {}", self.noun))
                .size(font::CAPTION)
                .style(|t: &iced::Theme| text::Style {
                    color: Some(theme::colors(t).text_muted()),
                }),
        );

        // 6. Load-more stays BELOW: it extends the table downwards, so it
        //    belongs where the rows run out.
        let more: Option<Element<'a, Message>> = (shown < total)
            .then_some(self.on_more)
            .flatten()
            .map(|on_more| {
                button(text("Show more").size(font::CAPTION))
                    .padding([space::XS as u16, space::SM as u16])
                    .style(button::text)
                    .on_press(on_more)
                    .into()
            });

        let mut table = column![controls, header, body];
        if let Some(more) = more {
            table = table.push(more);
        }
        table.spacing(space::SM).width(Length::Fill).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_test::simulator;

    #[derive(Clone)]
    struct Row {
        name: String,
        bytes: u64,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Msg {
        Sort(usize),
        Filter(String),
        More,
    }

    fn rows() -> Vec<Row> {
        vec![
            Row {
                name: "alpha".into(),
                bytes: 30,
            },
            Row {
                name: "bravo".into(),
                bytes: 10,
            },
            Row {
                name: "charlie".into(),
                bytes: 20,
            },
        ]
    }

    fn table<'a>() -> DataTable<'a, Row, Msg> {
        DataTable::new(vec![
            Column::fill("name", 2, |r: &Row| text(r.name.clone()).into())
                .sortable(|r: &Row| SortKey::Text(r.name.clone())),
            Column::fixed("bytes", 80.0, |r: &Row| text(r.bytes.to_string()).into())
                .sortable(|r: &Row| SortKey::Num(r.bytes as f64)),
        ])
        .searchable(|r: &Row| r.name.clone())
        .on_sort(Msg::Sort)
        .on_filter(Msg::Filter)
        .on_more(Msg::More)
        .noun("flows")
    }

    #[test]
    fn renders_rows_and_count() {
        let data = rows();
        let st = TableState::default();
        let mut ui = simulator(table().view(&data, &st));
        assert!(ui.find("alpha").is_ok());
        assert!(ui.find("charlie").is_ok());
        assert!(ui.find("showing 3 of 3 flows").is_ok());
    }

    /// The filter box must sit above the rows: on a long table, a control you
    /// have to scroll past the data to reach is the one you need most and can
    /// see least. Load-more stays below, where the rows run out.
    #[test]
    fn filter_sits_above_the_rows_and_load_more_below() {
        let data = rows();
        let st = TableState {
            limit: 1,
            ..Default::default()
        };
        let mut ui = simulator(table().view(&data, &st));

        let mut top = |t: &str| ui.find(t).unwrap_or_else(|_| panic!("{t}")).bounds().y;
        let filter = top("filter…");
        let count = top("showing 1 of 3 flows");
        let header = top("name");
        let first_row = top("alpha");
        let more = top("Show more");

        assert!(
            filter < header,
            "filter box must precede the column headers"
        );
        assert!(count < header, "the count travels with the filter box");
        assert!(header < first_row, "headers still precede the rows");
        assert!(
            more > first_row,
            "load-more extends the table downwards, so it stays at the bottom"
        );
    }

    #[test]
    fn clicking_header_emits_sort() {
        let data = rows();
        let st = TableState::default();
        let mut ui = simulator(table().view(&data, &st));
        let _ = ui.click("name");
        let msgs: Vec<Msg> = ui.into_messages().collect();
        assert!(msgs.contains(&Msg::Sort(0)));
    }

    #[test]
    fn filter_hides_non_matching_rows() {
        let data = rows();
        let mut st = TableState::default();
        st.set_filter("brav".into());
        let mut ui = simulator(table().view(&data, &st));
        assert!(ui.find("bravo").is_ok());
        assert!(ui.find("alpha").is_err());
        assert!(ui.find("showing 1 of 1 flows").is_ok());
    }

    #[test]
    fn limit_truncates_and_says_so_rather_than_cutting_silently() {
        let data = rows();
        let st = TableState {
            limit: 2,
            ..Default::default()
        };
        let mut ui = simulator(table().view(&data, &st));
        assert!(ui.find("showing 2 of 3 flows").is_ok());
        assert!(ui.find("Show more").is_ok());
    }
}
