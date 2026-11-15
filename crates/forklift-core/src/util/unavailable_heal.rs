//! Recording again, as unavailable, absent content the remote's host withholds (`heal`).
//!
//! A fetch records unavailable each file content it left out because the remote's host withholds
//! it and this store lacks it ([`unavailable`]). An entry that record lost, or one dropped from a
//! damaged record ([`unavailable::rebuild`]), turns that absence into a loss, although the host
//! still answers that it withholds the content. This records it again, on exactly the premise a
//! fetch records one on: the remote's handshake lists the hash withheld, with a statement and
//! chunk-list digest the record may hold, the handshake does not report it redacted and applied,
//! no redaction this warehouse applied names it, and the store lacks it. What history reaches is
//! not part of that premise; the caller passes only lost user file content, so nothing is recorded
//! that no history names, and no tree, chunk, parcel or meta content.

use std::collections::BTreeSet;
use crate::util::remote_utils::{self, HandshakeError, RemoteClient};
use crate::util::unavailable::{self, Unavailable};
use crate::util::{applied_redactions, file_utils};

/// Why [`record_withheld_lost`] recorded nothing because of the remote.
#[derive(Debug)]
pub enum RerecordError {
    /// The remote sent no response at all: running heal again once it answers may record them.
    NoResponse(String),
    /// The client, the handshake, or the remote's trust refused, in their own words.
    Refused(String),
}

/// Record unavailable again each of `lost` that the configured remote's host withholds (see the
/// module doc). `lost` must be absent user file content history still reaches
/// ([`crate::util::recovery_utils::LostObjects::absent_files`]), never every absent object: a fetch
/// records nothing else. No remote configured records nothing.
///
/// # Returns
/// * `Ok(Vec<String>)`     - The hashes recorded, sorted, possibly none.
/// * `Err(RerecordError)`  - The remote sent no response, or the client, the handshake or the
///                           remote's trust refused, or the record of applied redactions, the
///                           store or the unavailable record could not be read, or the record
///                           could not be written. Nothing was recorded, unless a write failed
///                           after its rename.
pub async fn record_withheld_lost(lost: &BTreeSet<String>) -> Result<Vec<String>, RerecordError> {
    if lost.is_empty() || !RemoteClient::is_configured() {
        return Ok(Vec::new());
    }

    let refused = RerecordError::Refused;
    let client = RemoteClient::for_warehouse().map_err(|e| refused(e.into()))?;
    let info = client.fetch_info_answered().await.map_err(|e| match e {
        HandshakeError::NoResponse(why) => RerecordError::NoResponse(why),
        HandshakeError::Answered(why) => RerecordError::Refused(why),
    })?;
    remote_utils::check_remote_trust(&client, &info, None).map_err(refused)?;

    // The handshake set what a walk through this client leaves out as withheld, on the filters a
    // fetch records by.
    let applied = applied_redactions::read_applied().map_err(refused)?;
    let mut entries: Vec<Unavailable> = Vec::new();

    for hash in lost {
        let Some((statement, digest)) = client.withheld_entry(hash) else { continue };
        if applied.contains(hash) || file_utils::does_object_exist(hash).map_err(refused)? {
            continue;
        }

        entries.push(Unavailable { hash: hash.clone(), statement, digest });
    }

    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let recorded: Vec<String> = entries.iter().map(|entry| entry.hash.clone()).collect();
    unavailable::record(entries).map_err(refused)?;

    Ok(recorded)
}
