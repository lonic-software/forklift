use serde::Serialize;

/// An operator is a person or an entity that interacts with the system.
///
/// The `identifier` is the operator's on-chain identity — an opaque string as far as
/// the chain is concerned. When none is configured, Forklift mints a UUID, so chains
/// are pseudonymous by default (zero PII in signed history); a hosting provider
/// supplies its own minted id, and a team that wants human-readable chains may set
/// any string, accepting that it is public in every clone, forever. Forklift holds no display
/// name for an operator: names come only from the remote's directory (`POST /v1/resolve`), which
/// decides who may see them.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Operator {
    /// The operator id recorded in parcels and office records. Opaque to the chain;
    /// a minted UUID by default (see `config_utils`).
    pub identifier: String,
}
