use std::collections::HashSet;
use forklift_core::error;
use forklift_core::util::held_heads;
use forklift_core::util::remote_utils::{self, RemoteClient};
use crate::output;

/// What the configured remote holds, for a command that frees what its history reaches.
pub(crate) struct RemoteHeld {
    /// The heads whose history the remote holds (see [`held_by_remote`]).
    pub heads: Vec<String>,

    /// The content the remote's host withholds, from the same handshake: the remote may lack it
    /// although its heads name it, so a copy here is never freed on the remote's account.
    pub withheld: HashSet<String>,
}

/// The heads whose history the configured remote holds, for a command that frees what that history
/// reaches (`scope prune`, `evict`), and the content its host withholds: its pallet heads, and for
/// each pallet deleted there the head its tombstone keeps, from a handshake whose trust is judged
/// as a lower would judge it but adopts nothing, and the earlier heads of the held-heads record
/// that the remote confirms its pallets still reach. The caller frees only what these heads reach, so it never runs without them: an
/// unreachable or untrusted remote refuses before anything changes.
///
/// A recorded head counts only when the remote answers that it holds it, so the record never frees
/// more than the current heads alone unless the remote affirms it; when the confirmation cannot be
/// had, the caller frees what the current heads alone allow, and this warns which pallet went
/// unanswered.
///
/// # Arguments
/// * `note` - Appended to every refusal: why the command asks, and that nothing was changed.
///
/// # Returns
/// * `Ok(RemoteHeld)` - The head of every pallet the remote holds or keeps a tombstone of, and the
///   confirmed recorded heads, beside the hashes its handshake lists withheld.
/// * `Err(String)`     - If no remote is configured, a partial warehouse's remote is not its
///   origin, it cannot be reached, its trust state is
///   refused, or the held-heads record does not parse; the message ends with `note`.
pub(crate) async fn held_by_remote(note: &str) -> Result<RemoteHeld, String> {
    let client = RemoteClient::for_warehouse().map_err(|e| error::append_to_message(e.into(), note))?;
    let info = client.fetch_info().await.map_err(|e| error::append_to_message(e, note))?;
    remote_utils::check_remote_trust(&client, &info, None).map_err(|e| error::append_to_message(e, note))?;

    // A deleted pallet's tombstone keeps its history on the remote as a live ref would.
    let held = info.held_pallet_heads();
    let confirmation = held_heads::confirmed_heads(&client, &held).await
        .map_err(|e| error::append_to_message(e, note))?;
    for (pallet, why) in &confirmation.unanswered {
        output::warn_holds_unanswered(pallet, why);
    }

    let withheld = info.withheld.iter().flatten().map(|item| item.hash.clone()).collect();

    Ok(RemoteHeld { heads: held.into_values().chain(confirmation.heads).collect(), withheld })
}
