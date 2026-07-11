use serde::Serialize;
use forklift_core::util::{audit_utils, office_utils, pallet_utils, scope_utils};
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};
use crate::output::{self, CommandOutput};

/// Handle the audit command: verify the warehouse's signed history offline.
///
/// * The office chain is verified first, from the genesis parcel forward: every office
///   parcel must be signed by a key that was active in the *previous* office state
///   (the genesis is self-signed by the key it introduces — the TOFU anchor).
/// * Then the given pallet (the current one by default) is verified: every parcel must
///   carry a valid signature by a tracked key. Parcels stacked before trust was
///   established are reported as legacy (unsigned) but do not fail the audit.
///
/// The verification itself lives in `forklift_core::util::audit_utils` — the reference
/// server runs the same checks before committing a ref update.
///
/// # Arguments
/// * `pallet` - The pallet to audit (`None` audits the current pallet).
///
/// # Returns
/// * `Ok(())`      - If the audit passed.
/// * `Err(String)` - If trust is not established, or any verification failed.
pub fn handle_command(pallet: Option<String>) -> Result<(), String> {
    let Some(anchor) = office_utils::read_trust_anchor()? else {
        return Err(
            "Trust is not established for this warehouse; there are no signatures to \
            audit. Establish it with \"office enroll\".".to_string()
        );
    };

    // A bare name is a working (user) pallet; `@office` reaches the office meta pallet.
    let pallet_ref = match pallet {
        Some(name) => PalletRef::parse(&name)?,
        None => PalletRef::user(pallet_utils::get_current_pallet_name()?),
    };

    let Some(office_head) = pallet_utils::get_meta_pallet_head(office_utils::OFFICE_PALLET_NAME)? else {
        return Err("Trust is established but the office pallet is missing.".to_string());
    };

    let office_state = audit_utils::verify_office_chain(&anchor, &office_head)?;

    let mut report = AuditReport {
        genesis: anchor.genesis.clone(),
        pallet: pallet_ref.to_wire(),
        pallet_verified: None,
        verified_parcels: 0,
        legacy_parcels: 0,
        scope: None,
    };

    // Auditing the office pallet is just the office-chain verification above; any other
    // pallet gets its signed history verified against the office state too.
    let is_office = pallet_ref.namespace == PalletNamespace::Meta
        && pallet_ref.name == office_utils::OFFICE_PALLET_NAME;

    if !is_office {
        let Some(head) = pallet_utils::get_pallet_head_in(pallet_ref.namespace, &pallet_ref.name)? else {
            return Err(format!("No pallet named \"{}\" exists (or it has nothing stacked).", pallet_ref.to_wire()));
        };

        let (verified, legacy) = audit_utils::verify_pallet_history(&head, &anchor, &office_state, None)?;

        report.pallet_verified = Some(true);
        report.verified_parcels = verified;
        report.legacy_parcels = legacy;

        // A sparse warehouse holds only its in-scope content, so signatures alone cannot speak
        // for what is on disk. Prove the fetched content present and re-hashed (sealing the rest
        // by the hash a signed parcel commits), and report the boundary so a sparse pass can
        // never read as a full-clone pass. A full store keeps today's signature-only audit
        // unchanged — object presence is a warehouse property, so the fetch scope is the seam.
        let fetch_scope = scope_utils::read_fetch_scope()?;
        if !fetch_scope.is_full() {
            audit_utils::verify_parcel_closure_scoped(&head, None, &fetch_scope)?;
            report.scope = Some(AuditScope::new(fetch_scope.prefixes().to_vec()));
        }
    }

    output::emit("audit", &report);

    Ok(())
}

/// The result of an offline audit: the office chain always, plus a working pallet's
/// parcel counts when one (not the office) was audited.
#[derive(Serialize)]
struct AuditReport {
    /// The genesis the office chain verified back to.
    genesis: String,

    /// The pallet that was audited.
    pallet: String,

    /// Whether a working pallet's history was audited (absent when only the office
    /// chain was — i.e. the audited pallet *is* the office).
    #[serde(skip_serializing_if = "Option::is_none")]
    pallet_verified: Option<bool>,

    /// How many signed parcels verified on the pallet.
    verified_parcels: usize,

    /// How many legacy (pre-trust, unsigned) parcels were tolerated.
    legacy_parcels: usize,

    /// Present only when the warehouse is sparse: the fetch-scope boundary the content audit
    /// ran against. A full clone omits it entirely, so a sparse pass is never mistaken for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<AuditScope>,
}

/// The scope boundary of a sparse warehouse's audit. Emitted only on a sparse run so that a
/// skimming reader (or agent) can never mistake a sparse pass — which verified content only
/// within the fetch scope — for a full-clone pass that verified all of it.
#[derive(Serialize)]
struct AuditScope {
    /// The warehouse fetch scope: the path prefixes whose content was fetched. Everything
    /// outside is sealed by hash, not downloaded.
    fetch_scope: Vec<String>,

    /// Signatures — the office chain and every parcel — are verified in full regardless of
    /// scope (parcels and their sidecars are always fully present).
    signatures: &'static str,

    /// In-scope content: every tree was re-hashed on read and every blob confirmed present.
    in_scope_content: &'static str,

    /// Out-of-scope content: sealed by the hash a signed parcel commits (unforgeable), verified
    /// when it is fetched.
    out_of_scope_content: &'static str,

    /// The scope boundary is advisory — a client choice, not enforced by the remote.
    enforcement: &'static str,
}

impl AuditScope {
    fn new(fetch_scope: Vec<String>) -> AuditScope {
        AuditScope {
            fetch_scope,
            signatures: "verified",
            in_scope_content: "verified",
            out_of_scope_content: "sealed",
            enforcement: "advisory",
        }
    }
}

impl CommandOutput for AuditReport {
    fn render_human(&self) {
        println!("Office chain verified back to genesis {}.", self.genesis);

        if self.pallet_verified != Some(true) {
            return;
        }

        println!(
            "Pallet \"{}\" verified: {} signed parcel(s) valid{}.",
            self.pallet,
            self.verified_parcels,
            if self.legacy_parcels > 0 {
                format!(", {} legacy parcel(s) predate trust and are unsigned", self.legacy_parcels)
            } else {
                String::new()
            }
        );

        // A distinct, self-contained boundary statement whenever the store is sparse — the one
        // line a full-clone audit never prints, so a partial verification is never read as a
        // complete one.
        if let Some(scope) = &self.scope {
            println!(
                "This warehouse is sparse; content outside the fetched scope ({}) is sealed by \
                hash, not downloaded.",
                scope.fetch_scope.join(", ")
            );
            println!(
                "Signatures are verified in full; within the fetched scope every tree was \
                re-hashed and every blob confirmed present."
            );
            println!(
                "Out-of-scope content is pinned to the hash a signed parcel commits — it cannot \
                be forged or substituted, only fetched. The boundary is advisory, not enforced \
                by the remote."
            );
        }
    }
}
