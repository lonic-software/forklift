//! A JSON reader for input whose every key must mean one thing.
//!
//! `serde_json`'s own `Value` keeps the last of two equal keys in an object and drops the first
//! without a word, so `{"all": [<filter>], "all": []}` reads as the empty conjunction: everything.
//! A reader that saw the first spelling and Forklift, which acts on the second, would disagree
//! about what the input asks for. [`parse_strict`] refuses such input instead. It keeps everything
//! else `serde_json::from_str` does, including its nesting limit and its refusal of trailing text.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

/// Parse `text` as one JSON value, refusing an object that names a key twice at any depth. The
/// error is `serde_json`'s message, which names the line and column.
pub fn parse_strict(text: &str) -> Result<Value, String> {
    serde_json::from_str::<Strict>(text).map(|Strict(value)| value).map_err(|error| error.to_string())
}

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Strict, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value).map(Value::Number).ok_or_else(|| E::custom("a number JSON cannot spell"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("the key \"{}\" appears twice in one object", key)));
            }
            let Strict(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_strict;
    use serde_json::json;

    #[test]
    fn a_key_spelled_twice_is_refused_at_any_depth() {
        for text in [
            r#"{"all": [], "all": []}"#,
            r#"{"not": {"field": "tag", "op": "eq", "value": "a", "value": "b"}}"#,
            r#"[1, {"a": {"b": 1, "b": 1}}]"#,
        ] {
            let error = parse_strict(text).unwrap_err();
            assert!(error.contains("appears twice"), "{text}: {error}");
        }
    }

    #[test]
    fn every_other_value_reads_as_serde_json_reads_it() {
        let text = r#"{"a": [true, false, null, -3, 18446744073709551615, 1.5, "é"], "b": {"a": 1}}"#;
        assert_eq!(parse_strict(text).unwrap(), serde_json::from_str::<serde_json::Value>(text).unwrap());
        assert_eq!(parse_strict(r#"{"a": 1, "b": 1}"#).unwrap(), json!({"a": 1, "b": 1}));
    }

    #[test]
    fn trailing_text_and_runaway_nesting_stay_refused() {
        assert!(parse_strict(r#"{"a": 1} x"#).is_err());
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(parse_strict(&deep).unwrap_err().contains("recursion limit"));
    }
}
