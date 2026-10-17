use crate::enums::object::compact_parcel_version::CompactParcelVersion;
use crate::model::parcel::Parcel;
use crate::util::{byte_utils, object_utils};

/// Parse a compact parcel object.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the header section.
/// * `content` - The bytes of the parcel object.
///
/// # Returns
/// * `Ok(Parcel)`  - The parsed parcel object.
/// * `Err(String)` - The error message.
pub fn parse_compact_parcel(offset: usize, content: &[u8]) -> Result<Parcel, String> {
    let mut cursor: usize = 0;

    let version_code = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })
        .map_err(|e| format!("Failed to parse compact parcel version: {}", e))?;

    let version = CompactParcelVersion::from_code(version_code)?;
    let parcel = version.get_parser()(offset + cursor, content)?;

    // Every hash a parcel names is later printed, walked and turned into a store path, and the
    // parcel may have arrived from a remote: one that is not hash-shaped is refused here, once,
    // rather than trusted by every reader.
    if !object_utils::is_object_hash(&parcel.tree_hash) {
        return Err("The parcel's tree hash is not an object hash.".to_string());
    }

    if parcel.parents.iter().any(|parent| !object_utils::is_object_hash(parent)) {
        return Err("A parent of the parcel is not an object hash.".to_string());
    }

    Ok(parcel)
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use crate::builder::object::parcel::parcel_object_builder::ParcelObjectBuilder;
    use crate::enums::parcel_action_type::ParcelActionType;
    use crate::model::operator::Operator;
    use crate::model::parcel_action::ParcelAction;

    fn action(description: Option<&str>) -> ParcelAction {
        ParcelAction {
            operator: Operator {
                identifier: "1a2b3c4d-0000-4000-8000-1234567890ab".to_string(),
                name: "Máté".to_string(),
            },
            action: ParcelActionType::Author,
            description: description.map(|d| d.to_string()),
            timestamp: chrono::Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap(),
        }
    }

    fn round_trip(parcel: &Parcel) -> Parcel {
        let object = ParcelObjectBuilder::build_compact(parcel);
        parse_compact_parcel(0, &object.content).unwrap()
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let parcel = Parcel {
            tree_hash: "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc".to_string(),
            parents: vec!["aa11".repeat(16), "bb22".repeat(16)],
            actions: vec![action(Some("added the thing"))],
            description: Some("A parcel description.".to_string()),
        };

        let parsed = round_trip(&parcel);

        assert_eq!(parsed.tree_hash, parcel.tree_hash);
        assert_eq!(parsed.parents, parcel.parents);
        assert_eq!(parsed.actions.len(), 1);
        // Only the opaque operator id crosses the wire; display data never does.
        assert_eq!(parsed.actions[0].operator.identifier, "1a2b3c4d-0000-4000-8000-1234567890ab");
        assert_eq!(parsed.actions[0].operator.name, "1a2b3c4d-0000-4000-8000-1234567890ab");
        assert_eq!(parsed.actions[0].description.as_deref(), Some("added the thing"));
        assert_eq!(parsed.actions[0].timestamp, parcel.actions[0].timestamp);
        assert_eq!(parsed.description.as_deref(), Some("A parcel description."));
    }

    #[test]
    fn round_trip_preserves_missing_descriptions_as_none() {
        let parcel = Parcel {
            tree_hash: "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc".to_string(),
            parents: vec![],
            actions: vec![action(None)],
            description: None,
        };

        let parsed = round_trip(&parcel);

        assert_eq!(parsed.actions[0].description, None);
        assert_eq!(parsed.description, None);
    }

    #[test]
    fn round_trip_preserves_hostile_descriptions() {
        // Action descriptions may contain new lines and EOT bytes; parcel descriptions
        // routinely contain new lines (multi-paragraph messages).
        let parcel = Parcel {
            tree_hash: "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc".to_string(),
            parents: vec![],
            actions: vec![action(Some("line one\nline two\u{3}with EOT"))],
            description: Some("Title\n\nBody paragraph.".to_string()),
        };

        let parsed = round_trip(&parcel);

        assert_eq!(parsed.actions[0].description.as_deref(), Some("line one\nline two\u{3}with EOT"));
        assert_eq!(parsed.description.as_deref(), Some("Title\n\nBody paragraph."));
    }

    #[test]
    fn a_hash_that_is_not_hash_shaped_is_refused() {
        let good = "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc".to_string();
        let bad = format!("{}\x1b[2J", &good[..60]);

        let parse = |tree_hash: String, parents: Vec<String>| {
            let parcel = Parcel { tree_hash, parents, actions: vec![action(None)], description: None };
            parse_compact_parcel(0, &ParcelObjectBuilder::build_compact(&parcel).content)
        };

        assert!(parse(good.clone(), vec![good.clone()]).is_ok());
        assert!(parse(bad.clone(), vec![]).err().unwrap().contains("tree hash"));
        assert!(parse(good.clone(), vec![good.clone(), bad]).err().unwrap().contains("parent"));
        assert!(parse(good.to_uppercase(), vec![]).is_err(), "only the canonical spelling is a hash");
    }
}
