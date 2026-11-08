//! Rows that are not accounts, in every format a list of accounts has.
//!
//! The account renderers know the six fields of a `User`. `snob snapshots`
//! (one row per capture) and `snob changes` (one row per change) have columns
//! of their own, and both want the same six formats with the same rules: a
//! drawn table on a terminal, tab-separated lines down a pipe or into a file
//! with no format named, JSON objects keyed by column, csv, Markdown, and a
//! spreadsheet. One renderer for both, so the two cannot answer alike in five
//! formats and differently in the sixth.
//!
//! **Every text cell passes `printable`** whatever its column: a username is
//! another person's string, and this renderer does not ask which cells hold
//! one. A moment is epoch seconds in JSON and csv, as everywhere else, and a
//! date a person reads in the table, Markdown and the spreadsheet.

use anyhow::Result;
use comfy_table::{ContentArrangement, Table, presets};
use serde_json::{Map, Value as Json};
use snob_core::Epoch;
use snob_core::model::printable;

use super::{Presentation, Rendered};
use crate::cli::Format;

/// What a cell holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Text(String),
    Number(u64),
    Flag(bool),
    Moment(Epoch),
    Empty,
}

impl Value {
    /// The text, or nothing, as a cell holding someone else's string.
    pub(crate) fn text(text: Option<&str>) -> Self {
        text.map_or(Self::Empty, |text| Self::Text(printable(text)))
    }

    fn json(&self) -> Json {
        match self {
            Self::Text(text) => Json::String(printable(text)),
            Self::Number(n) => Json::from(*n),
            Self::Flag(flag) => Json::Bool(*flag),
            Self::Moment(at) => Json::from(at.get()),
            Self::Empty => Json::Null,
        }
    }

    /// As a csv field: what the JSON says, an empty field for nothing.
    fn field(&self) -> String {
        match self {
            Self::Text(text) => printable(text),
            Self::Number(n) => n.to_string(),
            Self::Flag(flag) => flag.to_string(),
            Self::Moment(at) => at.get().to_string(),
            Self::Empty => String::new(),
        }
    }

    /// As a person reads it: a moment in the local zone.
    fn shown(&self) -> String {
        match self {
            Self::Moment(at) => crate::report::stored_on(*at),
            Self::Flag(true) => "yes".into(),
            Self::Flag(false) => "no".into(),
            other => other.field(),
        }
    }
}

/// A header and its rows. `columns` pairs each JSON key with the title a
/// table shows for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Grid {
    pub(crate) columns: Vec<(&'static str, &'static str)>,
    pub(crate) rows: Vec<Vec<Value>>,
}

impl Grid {
    /// Each row as a JSON object keyed by column, the shape `--format json`
    /// prints an array of and `ndjson` one per line.
    pub(crate) fn objects(&self) -> Vec<Json> {
        self.rows
            .iter()
            .map(|row| {
                let mut object = Map::new();
                for ((key, _), value) in self.columns.iter().zip(row) {
                    object.insert((*key).to_string(), value.json());
                }
                Json::Object(object)
            })
            .collect()
    }

    /// The grid in `format`. JSON is the array of [`Grid::objects`]; a caller
    /// with a richer document for JSON renders that itself and hands the
    /// other formats here.
    pub(crate) fn render(&self, format: Format, presentation: Presentation) -> Result<Rendered> {
        Ok(Rendered::Text(match format {
            Format::Table if presentation.interactive => self.table(presentation),
            Format::Table => self.lines(),
            Format::Json => {
                let mut text = serde_json::to_string_pretty(&self.objects())?;
                text.push('\n');
                text
            }
            Format::Ndjson => {
                let mut text = String::new();
                for object in self.objects() {
                    text.push_str(&serde_json::to_string(&object)?);
                    text.push('\n');
                }
                text
            }
            Format::Csv => self.csv()?,
            Format::Md => self.markdown(),
            Format::Xlsx => return self.workbook(),
        }))
    }

    fn titles(&self) -> Vec<&'static str> {
        self.columns.iter().map(|(_, title)| *title).collect()
    }

    /// The drawn table. Nothing for no rows: what the command says on
    /// standard error already counts them.
    fn table(&self, presentation: Presentation) -> String {
        if self.rows.is_empty() {
            return String::new();
        }
        let mut table = Table::new();
        table
            .load_preset(presets::UTF8_FULL_CONDENSED)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(super::table::header_cells(&self.titles(), presentation));
        if let Some(width) = presentation.width {
            table.set_width(width);
        }
        for row in &self.rows {
            table.add_row(row.iter().map(Value::shown).collect::<Vec<_>>());
        }
        let mut drawn = table.to_string();
        drawn.push('\n');
        drawn
    }

    /// Tab-separated, header first, for a pipe that asked for no format:
    /// what `cut` and `awk` read.
    fn lines(&self) -> String {
        let mut text = self
            .columns
            .iter()
            .map(|(key, _)| *key)
            .collect::<Vec<_>>()
            .join("\t");
        text.push('\n');
        for row in &self.rows {
            let fields: Vec<String> = row
                .iter()
                .map(|value| value.field().replace(['\t', '\n'], " "))
                .collect();
            text.push_str(&fields.join("\t"));
            text.push('\n');
        }
        text
    }

    fn csv(&self) -> Result<String> {
        let header: Vec<&str> = self.columns.iter().map(|(key, _)| *key).collect();
        let rows: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|row| row.iter().map(Value::field).collect())
            .collect();
        super::csv::many_rows(&header, &rows)
    }

    fn markdown(&self) -> String {
        let titles = self.titles();
        let mut text = format!("| {} |\n", titles.join(" | "));
        text.push_str(&format!("|{}\n", " --- |".repeat(titles.len())));
        for row in &self.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|value| super::md::escape(&value.shown()))
                .collect();
            text.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        text
    }

    fn workbook(&self) -> Result<Rendered> {
        #[cfg(feature = "xlsx")]
        {
            use super::xlsx::Cell;
            let header: Vec<&str> = self.columns.iter().map(|(key, _)| *key).collect();
            let rows: Vec<Vec<Cell>> = self
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| match value {
                            Value::Text(text) => Cell::Text(printable(text)),
                            Value::Number(n) => Cell::Number(*n as f64),
                            Value::Flag(flag) => Cell::Bool(*flag),
                            Value::Moment(at) => Cell::DateTime(*at),
                            Value::Empty => Cell::Empty,
                        })
                        .collect()
                })
                .collect();
            Ok(Rendered::Bytes(super::xlsx::many_rows_workbook(
                &header, &rows,
            )?))
        }
        #[cfg(not(feature = "xlsx"))]
        anyhow::bail!("this build of snob was made without the \"xlsx\" format");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Grid {
        Grid {
            columns: vec![("id", "Id"), ("name", "Name"), ("at", "When")],
            rows: vec![
                vec![
                    Value::Number(7),
                    Value::text(Some("a\u{202e}b")),
                    Value::Moment(Epoch::new(1_700_000_000)),
                ],
                vec![Value::Number(8), Value::Empty, Value::Empty],
            ],
        }
    }

    fn plain() -> Presentation {
        Presentation {
            interactive: false,
            hyperlinks: false,
            color: false,
            width: Some(80),
        }
    }

    fn text(rendered: Rendered) -> String {
        match rendered {
            Rendered::Text(text) => text,
            Rendered::Bytes(_) => panic!("not text"),
        }
    }

    #[test]
    fn json_is_one_object_per_row_keyed_by_column_with_moments_in_seconds() {
        let json: Json =
            serde_json::from_str(&text(grid().render(Format::Json, plain()).unwrap())).unwrap();
        assert_eq!(json[0]["id"], 7);
        assert_eq!(json[0]["name"], "ab", "filtered of the override");
        assert_eq!(json[0]["at"], 1_700_000_000);
        assert!(json[1]["name"].is_null());
    }

    #[test]
    fn down_a_pipe_with_no_format_it_is_tab_separated_with_a_header() {
        let lines = text(grid().render(Format::Table, plain()).unwrap());
        let mut lines = lines.lines();
        assert_eq!(lines.next(), Some("id\tname\tat"));
        assert_eq!(lines.next(), Some("7\tab\t1700000000"));
        assert_eq!(lines.next(), Some("8\t\t"));
    }

    #[test]
    fn csv_and_markdown_have_a_header_and_a_line_per_row() {
        let csv = text(grid().render(Format::Csv, plain()).unwrap());
        assert_eq!(csv.lines().count(), 3, "{csv}");
        let md = text(grid().render(Format::Md, plain()).unwrap());
        assert!(md.starts_with("| Id | Name | When |"), "{md}");
        assert_eq!(md.lines().count(), 4, "{md}");
    }
}
