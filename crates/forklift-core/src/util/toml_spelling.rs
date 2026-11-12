//! The one TOML spelling Forklift writes for a signed record: a cell envelope (`meta_tree`) and
//! an office user or key record (`office_utils`).
//!
//! These records are signed or content-addressed, so their decoders accept only the bytes this
//! writer produces for the data they parsed to; a second spelling of one record would be a
//! second byte string carrying the first one's meaning. That makes this writer part of the
//! on-disk format, which is why it is written here rather than left to a TOML library: a
//! library's choice of quote style is free to change between versions, and every record the
//! previous version wrote would then fail to read. The rules are those in
//! `docs/format/TRACKED_METADATA.md` ("One spelling per record"):
//!
//! - one `key = value` per line, `\n` line endings, a final newline, nothing else on any line;
//! - a key bare when it is non-empty and only `A-Z a-z 0-9 _ -`, otherwise a quoted string;
//! - a string in double quotes, `"` and `\` escaped as `\"` and `\\`, newline, tab, carriage
//!   return, backspace and form feed as `\n \t \r \b \f`, any other control character as
//!   `\uXXXX` (upper-case hex), everything else as itself;
//! - an integer in decimal, `-` for a negative one and no `+`; a boolean as `true` or `false`;
//! - an array as `[a, b]`, an inline table as `{ k = v, k2 = v2 }`, empty ones as `[]` and `{}`.
//!
//! Floats and date-times have more than one spelling of one value, so they are refused.

use toml_edit::{Item, Table, Value};

/// Append `key` in its one spelling.
pub(crate) fn write_key(out: &mut String, key: &str) {
    if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        out.push_str(key);
    } else {
        write_string(out, key);
    }
}

/// Append `text` as a quoted string in its one spelling.
pub(crate) fn write_string(out: &mut String, text: &str) {
    out.push('"');

    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c < ' ' || c == '\u{7f}' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }

    out.push('"');
}

/// Append `key = "text"` and a newline.
pub(crate) fn write_string_line(out: &mut String, key: &str, text: &str) {
    write_key(out, key);
    out.push_str(" = ");
    write_string(out, text);
    out.push('\n');
}

/// Append `key = number` and a newline.
pub(crate) fn write_integer_line(out: &mut String, key: &str, number: i64) {
    write_key(out, key);
    out.push_str(&format!(" = {}\n", number));
}

/// Append `key = ["a", "b"]` and a newline.
pub(crate) fn write_strings_line<'a>(out: &mut String, key: &str, texts: impl IntoIterator<Item = &'a str>) {
    write_key(out, key);
    out.push_str(" = [");

    for (index, text) in texts.into_iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }

        write_string(out, text);
    }

    out.push_str("]\n");
}

/// Append every entry of `table` as a `key = value` line, keys sorted by their text and every
/// nested table written inline. `Err` names the kind of value that has no one spelling (`"a
/// float"`, `"a date-time"`) and the top-level key holding it.
pub(crate) fn write_sorted_entries(out: &mut String, table: &Table) -> Result<(), (String, &'static str)> {
    for (key, item) in sorted_items(table) {
        let mut value = String::new();
        write_item(&mut value, item).map_err(|kind| (key.to_string(), kind))?;
        write_key(out, key);
        out.push_str(" = ");
        out.push_str(&value);
        out.push('\n');
    }

    Ok(())
}

fn sorted_items(table: &Table) -> Vec<(&str, &Item)> {
    let mut items: Vec<(&str, &Item)> = table.iter().filter(|(_, item)| !item.is_none()).collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    items
}

fn write_item(out: &mut String, item: &Item) -> Result<(), &'static str> {
    match item {
        Item::None => Ok(()),
        Item::Value(value) => write_value(out, value),
        Item::Table(table) => write_inline(out, sorted_items(table).into_iter().map(|(key, item)| (key, Entry::Item(item)))),
        Item::ArrayOfTables(tables) => {
            out.push('[');

            for (index, table) in tables.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }

                write_item(out, &Item::Table(table.clone()))?;
            }

            out.push(']');
            Ok(())
        }
    }
}

enum Entry<'a> {
    Item(&'a Item),
    Value(&'a Value),
}

fn write_inline<'a>(out: &mut String, entries: impl Iterator<Item = (&'a str, Entry<'a>)>) -> Result<(), &'static str> {
    let entries: Vec<_> = entries.collect();

    if entries.is_empty() {
        out.push_str("{}");
        return Ok(());
    }

    out.push_str("{ ");

    for (index, (key, entry)) in entries.into_iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }

        write_key(out, key);
        out.push_str(" = ");

        match entry {
            Entry::Item(item) => write_item(out, item)?,
            Entry::Value(value) => write_value(out, value)?,
        }
    }

    out.push_str(" }");
    Ok(())
}

fn write_value(out: &mut String, value: &Value) -> Result<(), &'static str> {
    match value {
        Value::String(text) => write_string(out, text.value()),
        Value::Integer(number) => out.push_str(&number.value().to_string()),
        Value::Boolean(flag) => out.push_str(if *flag.value() { "true" } else { "false" }),
        Value::Array(array) => {
            out.push('[');

            for (index, value) in array.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }

                write_value(out, value)?;
            }

            out.push(']');
        }
        Value::InlineTable(table) => {
            let mut entries: Vec<(&str, &Value)> = table.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            return write_inline(out, entries.into_iter().map(|(key, value)| (key, Entry::Value(value))));
        }
        Value::Float(_) => return Err("a float"),
        Value::Datetime(_) => return Err("a date-time"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(text: &str) -> String {
        let mut out = String::new();
        write_string(&mut out, text);
        out
    }

    /// Golden spellings, one per escaping rule. These bytes are part of the format: a change
    /// here changes the hash of every record that holds such a string.
    #[test]
    fn a_string_has_one_spelling() {
        assert_eq!(string("plain"), "\"plain\"");
        assert_eq!(string(""), "\"\"");
        assert_eq!(string("it's"), "\"it's\"");
        assert_eq!(string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(string("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(string("two\nlines\r\n"), "\"two\\nlines\\r\\n\"");
        assert_eq!(string("tab\tbs\u{8}ff\u{c}"), "\"tab\\tbs\\bff\\f\"");
        assert_eq!(string("\u{1}\u{1f}\u{7f}"), "\"\\u0001\\u001F\\u007F\"");
        assert_eq!(string("'''\"\"\""), "\"'''\\\"\\\"\\\"\"");
        assert_eq!(string("ünï €"), "\"ünï €\"");

        let mut key = String::new();
        ["bare_KEY-9", "a b", "", "ü"].iter().for_each(|k| { write_key(&mut key, k); key.push(' '); });
        assert_eq!(key, "bare_KEY-9 \"a b\" \"\" \"ü\" ");
    }

    /// Whatever this writer emits parses back to the same data.
    #[test]
    fn written_entries_parse_back() {
        let source = "z = [1, -2]\n\"a b\" = { y = true, x = \"q\\\"\\u0001\" }\nm = {}\n";
        let table = source.parse::<toml_edit::DocumentMut>().unwrap().as_table().clone();
        let mut out = String::new();
        write_sorted_entries(&mut out, &table).unwrap();
        assert_eq!(out, "\"a b\" = { x = \"q\\\"\\u0001\", y = true }\nm = {}\nz = [1, -2]\n");

        let again = out.parse::<toml_edit::DocumentMut>().unwrap().as_table().clone();
        let mut twice = String::new();
        write_sorted_entries(&mut twice, &again).unwrap();
        assert_eq!(twice, out);

        let floats = "f = 1.0\n".parse::<toml_edit::DocumentMut>().unwrap().as_table().clone();
        assert_eq!(write_sorted_entries(&mut String::new(), &floats), Err(("f".to_string(), "a float")));
    }
}
