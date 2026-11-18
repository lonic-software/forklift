//! Recording again, as unavailable, absent content the remote's host withholds (`heal`).
//! The remote's handshake ([`handshake`]) is read once for this and for the redactions `heal`
//! applies again, whose remote report it carries.
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
use crate::model::remote::WarehouseInfo;
use crate::util::applied_redactions::RemoteReport;
use crate::util::remote_utils::{self, HandshakeError, RemoteClient};
use crate::util::unavailable::{self, Unavailable};
use crate::util::{applied_redactions, file_utils};

/// Why the remote's handshake, or a step after it, gave `heal` nothing to act on.
#[derive(Debug)]
pub enum RemoteError {
    /// The remote sent no response at all: running heal again once it answers may act on it.
    NoResponse(String),
    /// The client, the handshake, or the remote's trust refused, or a read or write after it
    /// failed, in their own words.
    Refused(String),
}

/// The configured remote's handshake, read once for every step of `heal` that asks it, and only
/// once its trust is checked.
pub struct Answered {
    client: RemoteClient,
    info: WarehouseInfo,
}

impl Answered {
    /// What the remote reports applied, which a redaction this warehouse applies again waits for.
    pub fn report(&self) -> RemoteReport {
        RemoteReport::of(self.client.url(), &self.info)
    }
}

/// Read the configured remote's handshake and check its trust.
///
/// # Returns
/// * `Ok(Some(Answered))` - The remote answered, and its trust holds.
/// * `Ok(None)`           - No remote is configured.
/// * `Err(RemoteError)`   - The remote sent no response, or the client, the handshake or the
///                          remote's trust refused.
pub async fn handshake() -> Result<Option<Answered>, RemoteError> {
    if !RemoteClient::is_configured() {
        return Ok(None);
    }

    let refused = RemoteError::Refused;
    let client = RemoteClient::for_warehouse().map_err(|e| refused(e.into()))?;
    let info = client.fetch_info_answered().await.map_err(|e| match e {
        HandshakeError::NoResponse(why) => RemoteError::NoResponse(why),
        HandshakeError::Answered(why) => RemoteError::Refused(why),
    })?;
    remote_utils::check_remote_trust(&client, &info, None, crate::util::office_utils::TrustReadMode::Checked).map_err(refused)?;

    Ok(Some(Answered { client, info }))
}

/// Record unavailable again each of `lost` that the host of `remote`, the configured remote, whose
/// handshake [`handshake`] read, withholds (see the module doc). `lost` must be absent user file
/// content history still reaches ([`crate::util::recovery_utils::LostObjects::absent_files`]),
/// never every absent object: a fetch records nothing else.
///
/// # Returns
/// * `Ok(Vec<String>)`   - The hashes recorded, sorted, possibly none.
/// * `Err(RemoteError)`  - The record of applied redactions, the store or the unavailable record
///                         could not be read, or the record could not be written. Nothing was
///                         recorded, unless a write failed after its rename.
pub fn record_withheld_lost(lost: &BTreeSet<String>, remote: &Answered) -> Result<Vec<String>, RemoteError> {
    let refused = RemoteError::Refused;

    // The handshake set what a walk through this client leaves out as withheld, on the filters a
    // fetch records by.
    let applied = applied_redactions::read_applied().map_err(refused)?;
    let mut entries: Vec<Unavailable> = Vec::new();

    for hash in lost {
        let Some((statement, digest)) = remote.client.withheld_entry(hash) else { continue };
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
