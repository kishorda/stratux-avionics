//! A minimal RFC 4180 reader, for the two OurAirports files.
//!
//! Hand-rolled rather than pulled in, because the whole requirement is "split fields, honour
//! quotes" over two files whose shape is known, and because the traps are worth naming in one
//! place. OurAirports quotes any field containing a comma and escapes an embedded quote by
//! doubling it — airport names carry both, e.g. `"Martha's Vineyard, \"MVY\""`.
//!
//! Splitting on `,` instead would silently shift every column after the first comma-bearing name,
//! which is the kind of fault that produces plausible output: coordinates would still parse, they
//! would just belong to the wrong field.

use std::collections::HashMap;

use anyhow::{anyhow, Result};

pub struct Reader {
    columns: HashMap<String, usize>,
    rows: Vec<Vec<String>>,
}

impl Reader {
    /// Parse a whole file. These are 13 MB at the largest, read once on a dev machine, so there is
    /// nothing to gain from streaming and a good deal of clarity to lose.
    pub fn parse(text: &str) -> Result<Self> {
        let mut rows = split_records(text);
        if rows.is_empty() {
            return Err(anyhow!("no header row"));
        }
        let header = rows.remove(0);
        let columns = header
            .into_iter()
            .enumerate()
            .map(|(i, name)| (name, i))
            .collect();
        Ok(Self { columns, rows })
    }

    /// Index of a named column, failing loudly if it is gone.
    ///
    /// Resolved once per column rather than per row, and by name rather than by position, because
    /// OurAirports has added columns before. A positional read would keep working and start
    /// returning the neighbouring field.
    pub fn column(&self, name: &str) -> Result<usize> {
        self.columns
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("no {name:?} column; the source schema has changed"))
    }

    pub fn rows(&self) -> impl Iterator<Item = &Vec<String>> {
        self.rows.iter()
    }
}

/// Field at `index`, or an empty string when the row is short.
pub fn field(row: &[String], index: usize) -> &str {
    row.get(index).map(String::as_str).unwrap_or("")
}

fn split_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if quoted => {
                // A doubled quote inside a quoted field is one literal quote.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            '"' => quoted = true,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            // A newline inside quotes is data, not a record separator. OurAirports comments carry
            // them; the two files read here should not, but the parser must not corrupt the file
            // if one appears.
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut row));
            }
            '\r' if !quoted => {}
            other => field.push(other),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        records.push(row);
    }
    records.retain(|r| r.len() > 1 || r.first().is_some_and(|f| !f.is_empty()));
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_commas_do_not_split_a_field() {
        // The trap this parser exists for. Splitting on ',' here shifts every later column by one
        // and the file still parses, which is the worst kind of wrong.
        let text = "id,name,lat\n1,\"Newark, Liberty\",40.69\n";
        let r = Reader::parse(text).unwrap();
        let row = r.rows().next().unwrap();
        assert_eq!(field(row, 1), "Newark, Liberty");
        assert_eq!(field(row, 2), "40.69");
    }

    #[test]
    fn doubled_quotes_become_one_literal_quote() {
        let text = "id,name\n1,\"Martha''s\"\n2,\"say \"\"hello\"\"\"\n";
        let r = Reader::parse(text).unwrap();
        let rows: Vec<_> = r.rows().collect();
        assert_eq!(field(rows[1], 1), "say \"hello\"");
    }

    #[test]
    fn empty_fields_stay_empty_rather_than_vanishing() {
        // Most OurAirports rows have empty iata_code and local_code. If an empty field collapsed,
        // every column after it would shift.
        let text = "a,b,c,d\n1,,,4\n";
        let r = Reader::parse(text).unwrap();
        let row = r.rows().next().unwrap();
        assert_eq!(row.len(), 4);
        assert_eq!(field(row, 1), "");
        assert_eq!(field(row, 3), "4");
    }

    #[test]
    fn columns_are_found_by_name_not_position() {
        let text = "ident,type,latitude_deg\nKMMU,medium_airport,40.79\n";
        let r = Reader::parse(text).unwrap();
        assert_eq!(r.column("latitude_deg").unwrap(), 2);
        assert!(
            r.column("elevation_ft").is_err(),
            "a missing column must fail loudly"
        );
    }

    #[test]
    fn a_newline_inside_quotes_is_data() {
        let text = "id,note\n1,\"line one\nline two\"\n2,plain\n";
        let r = Reader::parse(text).unwrap();
        let rows: Vec<_> = r.rows().collect();
        assert_eq!(rows.len(), 2, "the quoted newline split a record");
        assert_eq!(field(rows[0], 1), "line one\nline two");
    }

    #[test]
    fn crlf_and_a_missing_final_newline_both_parse() {
        let r = Reader::parse("a,b\r\n1,2\r\n3,4").unwrap();
        let rows: Vec<_> = r.rows().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(field(rows[1], 1), "4");
    }
}
