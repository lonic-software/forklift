//! Seeded byte mutation for the in-crate fuzz tests of the transport readers (packs, bundles) and
//! of the signed meta records (cells, office records), whose fixtures or decoders are
//! crate-private. `tests/fuzz_formats.rs` keeps its own copy for the public parse entry points.
//! The generator is splitmix64 with a fixed seed, so a failing case is reproducible from its seed
//! alone.

/// A deterministic splitmix64 stream.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`; `bound` must not be zero.
    pub(crate) fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    pub(crate) fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }
}

/// Apply one to three mutations to `bytes`: truncation, a bit flip, a random or zeroed run, a
/// boundary value written over a length-sized field, an insertion, a deletion, or a duplicated
/// run. Each targets a class of reader mistake: short reads, trusted lengths, unread tails.
pub(crate) fn mutate(bytes: &mut Vec<u8>, rng: &mut Rng) {
    for _ in 0..1 + rng.below(3) {
        if bytes.is_empty() {
            bytes.push(rng.byte());
            continue;
        }
        let at = rng.below(bytes.len());
        let run_end = (at + 1 + rng.below(16)).min(bytes.len());
        match rng.below(8) {
            0 => bytes.truncate(at),
            1 => bytes[at] ^= 1 << rng.below(8),
            2 => bytes[at..run_end].iter_mut().for_each(|byte| *byte = rng.byte()),
            3 => bytes[at..run_end].iter_mut().for_each(|byte| *byte = 0),
            4 => {
                // A boundary value over a 4- or 8-byte field, in either byte order.
                let width = if rng.below(2) == 0 { 4 } else { 8 };
                let value: u64 = [0, 1, u32::MAX as u64, u64::MAX, 1 << 31, 1 << 63][rng.below(6)];
                let mut field = value.to_be_bytes()[8 - width..].to_vec();
                if rng.below(2) == 0 {
                    field.reverse();
                }
                let end = (at + width).min(bytes.len());
                bytes[at..end].copy_from_slice(&field[..end - at]);
            }
            5 => {
                let inserted: Vec<u8> = (0..1 + rng.below(8)).map(|_| rng.byte()).collect();
                bytes.splice(at..at, inserted);
            }
            6 => {
                bytes.drain(at..run_end);
            }
            _ => {
                let run = bytes[at..run_end].to_vec();
                bytes.splice(run_end..run_end, run);
            }
        }
    }
}

/// The ways [`respell_toml`] rewrites a TOML text without changing the data it holds.
pub(crate) const RESPELLINGS: [&str; 12] = [
    "comment", "blank line", "spacing", "quoted key", "literal string", "unicode escape",
    "integer", "reordered keys", "dotted keys", "inline table", "line endings", "final newline",
];

/// Rewrite `text` by one of [`RESPELLINGS`], picked at random, into another spelling of the same
/// TOML data, or return it unchanged when that rewrite finds nothing to apply to. A rewrite may
/// also produce text that is no longer valid TOML; the caller compares data with [`toml_data`]
/// to tell a second spelling from damage.
pub(crate) fn respell_toml(text: &str, rng: &mut Rng) -> (&'static str, String) {
    let which = rng.below(RESPELLINGS.len());
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let line_count = lines.len();
    // A `key = value` line: its index, the key and the value.
    let assignments: Vec<(usize, String, String)> = lines.iter().enumerate()
        .filter_map(|(at, line)| {
            let (key, value) = line.trim_end_matches('\n').split_once(" = ")?;
            Some((at, key.to_string(), value.to_string()))
        })
        .collect();
    let pick = |rng: &mut Rng| (!assignments.is_empty()).then(|| assignments[rng.below(assignments.len())].clone());

    match which {
        0 => lines.insert(rng.below(line_count + 1), "# respelled\n".to_string()),
        1 => lines.insert(rng.below(line_count + 1), "\n".to_string()),
        2 => if let Some((at, key, value)) = pick(rng) {
            lines[at] = if rng.below(2) == 0 { format!("{}  =\t{}\n", key, value) } else { format!("{} = {} \n", key, value) };
        },
        3 => if let Some((at, key, value)) = pick(rng) {
            if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                lines[at] = format!("\"{}\" = {}\n", key, value);
            }
        },
        4 => if let Some((at, key, value)) = pick(rng) {
            let inner = value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"'));
            if let Some(inner) = inner.filter(|inner| !inner.contains(['\\', '\'', '"'])) {
                lines[at] = format!("{} = '{}'\n", key, inner);
            }
        },
        5 => if let Some((at, key, value)) = pick(rng) {
            if value.starts_with('"') {
                if let Some((offset, letter)) = value.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
                    let escaped = format!("{}\\u{:04X}{}", &value[..offset], letter as u32, &value[offset + 1..]);
                    lines[at] = format!("{} = {}\n", key, escaped);
                }
            }
        },
        6 => if let Some((at, key, value)) = pick(rng) {
            if let Ok(number) = value.parse::<i64>() {
                let respelled = match rng.below(3) {
                    0 if number >= 0 => format!("+{}", number),
                    1 if number >= 0 => format!("0x{:x}", number),
                    _ if number.unsigned_abs() >= 10 => format!("{}_{}", &value[..value.len() - 1], &value[value.len() - 1..]),
                    _ => format!("+{}", number),
                };
                lines[at] = format!("{} = {}\n", key, respelled);
            }
        },
        7 => {
            let adjacent: Vec<usize> = assignments.windows(2)
                .filter(|pair| pair[1].0 == pair[0].0 + 1)
                .map(|pair| pair[0].0)
                .collect();
            if !adjacent.is_empty() {
                let at = adjacent[rng.below(adjacent.len())];
                lines.swap(at, at + 1);
            }
        }
        8 | 9 => if let Some(header) = lines.iter().position(|line| line.starts_with('[')) {
            let table = lines[header].trim().trim_matches(['[', ']']).to_string();
            let body: Vec<String> = lines.drain(header..).skip(1).collect();
            if body.iter().all(|line| line.contains(" = ")) {
                if which == 8 {
                    lines.extend(body.iter().map(|line| format!("{}.{}", table, line)));
                } else {
                    let entries: Vec<&str> = body.iter().map(|line| line.trim_end_matches('\n')).collect();
                    lines.push(format!("{} = {{ {} }}\n", table, entries.join(", ")));
                }
            } else {
                lines.push(format!("[{}]\n", table));
                lines.extend(body);
            }
        },
        10 => return (RESPELLINGS[which], text.replace('\n', "\r\n")),
        _ => return (RESPELLINGS[which], match text.strip_suffix('\n') {
            Some(without) if rng.below(2) == 0 => without.to_string(),
            _ => format!("{}\n", text),
        }),
    }

    (RESPELLINGS[which], lines.concat())
}

/// The data a TOML text holds, as a string that ignores every spelling choice (key order,
/// quoting, escapes, number bases, decoration, inline versus header tables), or `None` when the
/// text is not TOML. Written independently of the encoders under test, so comparing two texts
/// with it says whether they spell the same data.
pub(crate) fn toml_data(text: &str) -> Option<String> {
    use toml_edit::{Item, Value};

    fn of_value(value: &Value) -> String {
        match value {
            Value::String(text) => format!("s{:?}", text.value()),
            Value::Integer(number) => format!("i{}", number.value()),
            Value::Float(number) => format!("f{:?}", number.value()),
            Value::Boolean(flag) => format!("b{}", flag.value()),
            Value::Datetime(when) => format!("d{}", when.value()),
            Value::Array(array) => format!("[{}]", array.iter().map(of_value).collect::<Vec<_>>().join(",")),
            Value::InlineTable(table) => table_of(table.iter().map(|(key, item)| (key.to_string(), of_value(item)))),
        }
    }

    fn of_item(item: &Item) -> String {
        match item {
            Item::None => "none".to_string(),
            Item::Value(inner) => of_value(inner),
            Item::Table(table) => table_of(table.iter().map(|(key, inner)| (key.to_string(), of_item(inner)))),
            Item::ArrayOfTables(tables) => format!("[{}]", tables.iter()
                .map(|table| table_of(table.iter().map(|(key, inner)| (key.to_string(), of_item(inner)))))
                .collect::<Vec<_>>().join(",")),
        }
    }

    fn table_of(entries: impl Iterator<Item = (String, String)>) -> String {
        let sorted: std::collections::BTreeMap<String, String> = entries.collect();
        format!("{{{}}}", sorted.iter().map(|(key, inner)| format!("{:?}:{}", key, inner)).collect::<Vec<_>>().join(","))
    }

    let document: toml_edit::DocumentMut = text.parse().ok()?;
    Some(of_item(document.as_item()))
}
