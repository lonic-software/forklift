use serde::Serialize;
use forklift_core::util::{audit_utils, office_utils, pallet_utils, scope_utils, sealed_utils};
use forklift_core::util::audit_utils::SealedTolerance;
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
/// * `full`   - Re-read every present chunk's bytes (re-hashing it on the content-addressed load)
///   and re-assemble each fully-present chunked file to verify `Blake3(assembled) ==
///   recipe.content_hash` — the one integrity claim a normal audit never checks. A normal audit
///   presence-checks chunks (bounded, no bytes re-read); `--full` is the stronger, slower level.
///
/// # Returns
/// * `Ok(())`      - If the audit passed.
/// * `Err(String)` - If trust is not established, or any verification failed.
pub fn handle_command(pallet: Option<String>, full: bool) -> Result<(), String> {
    let Some(anchor) = office_utils::read_trust_anchor()? else {
        return Err(
            "Trust is not established for this warehouse; there are no signatures to \
            audit. Establish it with \"office enroll\".".to_string()
        );
    };

    // A bare name is a working (user) pallet; `@office` reaches the office meta pallet.
    let pallet_ref = match pallet {
        Some(name) => PalletRef::parse(&name)?,
        None => PalletRef::user(pallet_utils::read_position()?.pallet),
    };

    let Some(office_head) = pallet_utils::get_meta_pallet_head(office_utils::OFFICE_PALLET_NAME)? else {
        return Err("Trust is established but the office pallet is missing.".to_string());
    };

    let office_state = audit_utils::verify_office_chain(&anchor, &office_head)?;

    let mut report = AuditReport {
        genesis: anchor.genesis.clone(),
        pallet: pallet_ref.to_wire(),
        pallet_verified: false,
        verified_parcels: 0,
        legacy_parcels: 0,
        full,
        scope: None,
    };

    // Auditing the office pallet is just the office-chain verification above; any other
    // pallet gets its signed history verified against the office state too.
    let is_office = pallet_ref.namespace == PalletNamespace::Meta
        && pallet_ref.name == office_utils::OFFICE_PALLET_NAME;

    if !is_office {
        let Some(head) = pallet_utils::get_pallet_head_in(pallet_ref.namespace, &pallet_ref.name)? else {
            return Err(pallet_utils::pallet_not_found_refusal(&pallet_ref).into());
        };

        let (verified, legacy) = audit_utils::verify_pallet_history(&head, &anchor, &office_state, &[]).map_err(String::from)?;

        report.pallet_verified = true;
        report.verified_parcels = verified;
        report.legacy_parcels = legacy;

        // A partial warehouse (sparse, or a cache) may lack content its history reaches, so
        // signatures alone cannot speak for what is on disk. Prove the fetched content present
        // and re-hashed (sealing the rest by the hash a signed parcel commits), and report the
        // boundary so a partial pass can never read as a full-clone pass. A full store keeps
        // today's signature-only audit unchanged — unless `--full` is asked, which re-reads every
        // present chunk and re-verifies each chunked file's content hash even on a full clone.
        // Object presence is a warehouse property, so the mode the warehouse marker records (and a
        // sparse warehouse's fetch scope) is the seam either way. Whenever the content audit runs, it counts and reports what
        // the sealed record held by hash alone; a signature-only pass makes no content claim, so
        // it has nothing of that kind to report.
        let fetch_scope = scope_utils::read_fetch_scope()?;
        let is_sparse = !fetch_scope.is_full();
        let is_cache = scope_utils::is_cache_mode()?;

        if is_sparse || full || is_cache {
            let record = sealed_utils::read_sealed()?;
            let sealed = SealedTolerance::new(&record);
            audit_utils::verify_parcel_closure_scoped(&head, None, &fetch_scope, full, Some(&sealed))?;
            report.scope = Some(AuditScope::new(
                is_sparse.then(|| fetch_scope.prefixes().to_vec()),
                is_cache,
                full,
                sealed.held(),
                sealed_utils::present_recorded(&record)?.len(),
            ));
        }
    }

    output::emit(&report);

    Ok(())
}

/// The result of an offline audit: the office chain always, plus a working pallet's
/// parcel counts when one (not the office) was audited.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct AuditReport {
    /// The genesis the office chain verified back to.
    genesis: String,

    /// The pallet that was audited.
    pallet: String,

    /// Whether a working pallet's history was audited (absent when only the office
    /// chain was — i.e. the audited pallet *is* the office).
    #[serde(skip_serializing_if = "crate::output::is_false")]
    pallet_verified: bool,

    /// How many signed parcels verified on the pallet.
    verified_parcels: usize,

    /// How many legacy (pre-trust, unsigned) parcels were tolerated.
    legacy_parcels: usize,

    /// Whether this was a `--full` audit: every present chunk's bytes were re-read and re-hashed,
    /// and each fully-present chunked file re-assembled to verify its recipe's content hash. A
    /// normal audit presence-checks chunks without re-reading them.
    full: bool,

    /// The content audit that ran: present when the warehouse is partial, sparse or a cache (to
    /// report what it may lack), or when `--full` re-verified content on a full clone. A normal full-clone audit is
    /// signature-only and omits it, so a partial or presence-only pass is never mistaken for a
    /// complete content re-verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<AuditScope>,
}

/// What a content audit checked, and (on a partial warehouse) what it sealed rather than verified.
/// Emitted so a skimming reader or agent can never mistake a sparse pass — which verified content
/// only within the fetch scope — or a cache pass — which verified only the content it holds — for
/// a full-clone pass, nor a presence-only pass for a `--full` re-read.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct AuditScope {
    /// The warehouse fetch scope: the path prefixes whose content was fetched. Present only on a
    /// sparse warehouse; a full clone omits it (nothing is out of scope). Everything outside is
    /// sealed by hash, not downloaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    fetch_scope: Option<Vec<String>>,

    /// The warehouse is a cache (franchised with `--cache`): it may lack content its history
    /// reaches, held by hash alone and counted in `held_by_hash`. Omitted on any other warehouse.
    #[serde(skip_serializing_if = "crate::output::is_false")]
    cache: bool,

    /// Signatures — the office chain and every parcel — are verified in full regardless of
    /// scope (parcels and their sidecars are always fully present).
    signatures: &'static str,

    /// In-scope content: every tree was re-hashed on read and every blob confirmed present, except
    /// the absent objects counted in `held_by_hash`.
    in_scope_content: &'static str,

    /// How many absent objects the warehouse's sealed record holds by hash alone: each is pinned to
    /// the hash a parent that is present commits, but is not on disk, and nothing beneath it was
    /// checked. Omitted when none.
    #[serde(skip_serializing_if = "crate::output::is_zero")]
    held_by_hash: usize,

    /// How many objects the sealed record names that are on disk all the same. That fails
    /// nothing: a recorded object that is present is checked like any other. `heal` removes the
    /// records it can, and notes each one it keeps. Omitted when none.
    #[serde(skip_serializing_if = "crate::output::is_zero")]
    present_and_recorded: usize,

    /// How a chunked file's chunks were checked: `presence-checked` in a normal audit (bounded, no
    /// bytes re-read), or, under `--full`, re-read and re-hashed with each fully-present file
    /// re-assembled to verify its recipe's content hash.
    chunks: &'static str,

    /// Out-of-scope content: sealed by the hash a signed parcel commits (unforgeable), verified
    /// when it is fetched. Present only on a sparse warehouse.
    #[serde(skip_serializing_if = "Option::is_none")]
    out_of_scope_content: Option<&'static str>,

    /// The scope boundary is advisory — a client choice, not enforced by the remote. Present only
    /// on a sparse warehouse.
    #[serde(skip_serializing_if = "Option::is_none")]
    enforcement: Option<&'static str>,
}

impl AuditScope {
    /// * `fetch_scope` - `Some(prefixes)` on a sparse warehouse (the sealed boundary), `None` on a
    ///   full clone.
    /// * `cache`       - Whether the warehouse is a cache.
    /// * `full`        - Whether `--full` re-read and re-verified chunks (vs. presence-checked).
    /// * `held`        - How many absent objects the sealed record held by hash alone.
    /// * `present`     - How many objects the sealed record names that the store holds.
    fn new(fetch_scope: Option<Vec<String>>, cache: bool, full: bool, held: usize, present: usize) -> AuditScope {
        let sparse = fetch_scope.is_some();

        AuditScope {
            fetch_scope,
            cache,
            signatures: "verified",
            in_scope_content: "verified",
            held_by_hash: held,
            present_and_recorded: present,
            chunks: if full {
                "re-read and re-hashed; each fully-present chunked file re-assembled to verify its content hash"
            } else {
                "presence-checked"
            },
            out_of_scope_content: sparse.then_some("sealed"),
            enforcement: sparse.then_some("advisory"),
        }
    }
}

impl CommandOutput for AuditReport {
    fn render_human(&self) {
        println!("Office chain verified back to genesis {}.", self.genesis);

        if !self.pallet_verified {
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

        // The stronger content level, stated plainly whenever it ran — so a presence-only pass is
        // never read as a full re-verification.
        if self.full {
            println!(
                "Full content re-verification: every present chunk was re-read and re-hashed, and \
                each fully-present chunked file was re-assembled to confirm its recorded content \
                hash. Blobs stay presence-checked."
            );
        }

        // A distinct, self-contained boundary statement whenever the store is sparse — the one
        // line a full-clone audit never prints, so a partial verification is never read as a
        // complete one.
        if let Some(AuditScope { fetch_scope: Some(prefixes), .. }) = &self.scope {
            println!(
                "This warehouse is sparse; content outside the fetched scope ({}) is sealed by \
                hash, not downloaded.",
                prefixes.join(", ")
            );
            println!(
                "Signatures are verified in full; within the fetched scope every tree was \
                re-hashed and every blob and chunk confirmed present{}.",
                if self.scope.as_ref().is_some_and(|scope| scope.held_by_hash > 0) {
                    ", except those held by hash below"
                } else {
                    ""
                }
            );
            println!(
                "Out-of-scope content is pinned to the hash a signed parcel commits — it cannot \
                be forged or substituted, only fetched. The boundary is advisory, not enforced \
                by the remote."
            );
        }

        if let Some(AuditScope { cache: true, held_by_hash, .. }) = &self.scope {
            println!(
                "This warehouse is a cache: it may lack content its history reaches. Signatures are \
                verified in full; every tree it holds was re-hashed and every blob and chunk it \
                names confirmed present{}.",
                if *held_by_hash > 0 { ", except those held by hash below" } else { "" }
            );
        }

        // Whatever the store's scope, objects held by hash alone are never passed over in silence.
        if let Some(AuditScope { held_by_hash: held @ 1.., .. }) = &self.scope {
            println!(
                "{} absent object(s) are held by hash alone: each is pinned to the hash a parent \
                that is present commits, but is not on disk, and nothing beneath it was checked.",
                held
            );
        }

        if let Some(AuditScope { present_and_recorded: present @ 1.., .. }) = &self.scope {
            println!(
                "{} object(s) on disk are still recorded as held by hash alone. That fails nothing, \
                since a recorded object on disk is checked like any other wherever it is reached. Run \"forklift heal\" to remove their records; it notes each one it must keep, and why.",
                present
            );
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<AuditReport>(),
    ]
}
