use chrono::Utc;
use serde::Serialize;
use forklift_core::model::operator::Operator;
use forklift_core::util::office_utils::{IdentityClass, OfficeState, RevocationReason, Role, TrustAnchor, UserRecord};
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::remote_utils::{self, RemoteClient};
use forklift_core::util::{config_utils, office_utils, pallet_utils, sign_utils};
use crate::output::{self, CommandOutput};

// The office command — the warehouse office manages the personnel records: users and
// keys as tracked metadata on the reserved "office" pallet. One public
// function per subcommand; the CLI surface itself is defined in `cli.rs`.

/// Enroll the configured operator and establish trust: the genesis office parcel
/// introduces the first user and key, self-signed by that key (the TOFU anchor).
///
/// When a remote is configured, its pallet heads join the trust boundary: the remote
/// may be ahead of this warehouse, and unsigned history it already has must stay
/// tolerated once the anchor reaches it — otherwise the pallet could never be lifted
/// again (trust is a one-way door and history is immutable).
pub async fn enroll(offline: bool, passphrase: bool) -> Result<(), String> {
    let operator = config_utils::get_operator()?;

    if office_utils::read_trust_anchor()?.is_some() {
        return Err(
            "Trust is already established for this warehouse. Ask an enrolled operator \
            to admit you: generate a keypair with \"office keygen\" and hand them the \
            public key.".to_string()
        );
    }

    // Everything reachable from the current pallet heads is the pre-trust history:
    // audit will allow it to be unsigned, and require signatures on everything else.
    let mut boundary: Vec<String> = Vec::new();

    for pallet in pallet_utils::list_pallets()? {
        if let Some(head) = pallet_utils::get_pallet_head(&pallet)? {
            boundary.push(head);
        }
    }

    let has_remote =
        config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?.is_some();

    if has_remote && !offline {
        let client = RemoteClient::from_config()?;

        let info = client.fetch_info().await.map_err(|e| format!(
            "{}\nEnroll includes the remote's pallet heads in the trust boundary, so it \
            must be reachable. Retry when it is, or pass --offline if the remote is \
            gone for good.",
            e
        ))?;

        if info.trust.is_some() {
            return Err(
                "The remote already has trust established. \"lower\" to adopt its \
                office, then ask an enrolled operator to admit you.".to_string()
            );
        }

        for head in info.pallets.values() {
            if !boundary.contains(head) {
                boundary.push(head.clone());
            }
        }
    }

    let now = Utc::now().timestamp();
    let (key_id, public_key) = generate_operator_key(&operator.identifier, passphrase)?;

    // The genesis key is the operator's identity root: self-endorsed (the
    // trust-on-first-use anchor of the identity), with a proof-of-possession like
    // every other key. The record carries only the opaque operator id — no display
    // data goes on-chain.
    let pop = office_utils::sign_key_pop(&key_id, &public_key, &operator.identifier)?;
    let root_key = office_utils::endorse_key(&public_key, &operator.identifier, &key_id, &pop, now)?;

    let state = OfficeState {
        users: vec![UserRecord {
            identifier: operator.identifier.clone(),
            enrolled_at: now,
            // The genesis operator administers the office they created.
            role: Role::Admin,
            pallets: Vec::new(),
            identity_root: key_id.clone(),
            // Enroll is a person establishing trust for their warehouse.
            class: IdentityClass::Human,
            supervisor: None,
        }],
        keys: vec![root_key],
    };

    let description = format!("Enrolled operator \"{}\" (genesis).", operator.identifier);
    let genesis = office_utils::stack_office_parcel(&state, &operator, description, &key_id)?;

    office_utils::write_trust_anchor(&TrustAnchor {
        genesis: genesis.clone(),
        enabled_at: now,
        boundary,
        prior_genesis: None,
        adopts: None,
    })?;

    output::message("office", format!(
        "Enrolled \"{}\" and established trust.\n\
        Genesis parcel:    {}\n\
        Identity root key: {}\n\n\
        From now on every parcel stacked in this warehouse must be signed. This cannot \
        be undone (a lockout is handled by archiving and re-preparing the warehouse).",
        operator.identifier, genesis, key_id
    ));

    Ok(())
}

/// Generate a keypair locally and print the public half plus its proof-of-possession
/// (the CSR of the enrollment flow, §8.6): the new key signs for the operator id
/// itself, so an admission cannot enroll a key the operator does not control — nor
/// attribute a consenting key to someone else.
pub fn keygen(passphrase: bool) -> Result<(), String> {
    let operator = config_utils::get_operator()?;
    let (key_id, public_key) = generate_operator_key(&operator.identifier, passphrase)?;
    let pop = office_utils::sign_key_pop(&key_id, &public_key, &operator.identifier)?;

    output::message("office", format!(
        "Generated a new keypair; the private key stays on this machine.\n\
        Key id:      {key_id}\n\
        Operator id: {identifier}\n\n\
        To be admitted, hand an office admin this line:\n\
        \x20 office admit {identifier} {public_key} {pop}\n\n\
        To link this as another device of an identity already enrolled as {identifier}, run\n\
        on a machine that holds one of its keys:\n\
        \x20 office link {public_key} {pop}",
        key_id = key_id, identifier = operator.identifier, public_key = public_key, pop = pop
    ));

    Ok(())
}

/// Admit another user: record them, their role, and their identity-root key — verified
/// by proof-of-possession and endorsed by the admitting admin's key (the scope of a
/// key-authorization equals the scope of the authorizer's authority, §8.6). The record
/// carries only the opaque operator id — no display data goes on-chain.
#[allow(clippy::too_many_arguments)]
pub fn admit(operator_id: &str,
             public_key: &str,
             pop: &str,
             role: &str,
             pallets: Vec<String>,
             class: IdentityClass,
             supervisor: Option<String>) -> Result<(), String> {
    let actor = config_utils::get_operator()?;
    let (mut state, signing_key_id, actor_id) = require_signing_actor(&actor)?;
    require_admin(&state, &actor_id)?;

    let role = Role::parse(role)?;

    // An agent must be bound to a supervising human (§7.1); a supervisor, when given,
    // must be an enrolled human (automation cannot supervise automation).
    if class == IdentityClass::Agent && supervisor.is_none() {
        return Err(
            "An agent must have a supervising human: pass --supervisor <operator>.".to_string()
        );
    }

    if let Some(supervisor) = &supervisor {
        match state.find_user(supervisor) {
            None => return Err(format!(
                "The supervisor \"{}\" is not enrolled in this office.", supervisor
            )),
            Some(user) if user.class != IdentityClass::Human => return Err(format!(
                "The supervisor \"{}\" is a {}, not a human; only a human can supervise.",
                supervisor, user.class.as_str()
            )),
            Some(_) => {}
        }
    }

    let key_bytes = sign_utils::from_hex(public_key)
        .map_err(|_| "The public key is not valid hex.".to_string())?;

    if key_bytes.len() != 32 {
        return Err("The public key must be 32 bytes (64 hex characters).".to_string());
    }

    if state.find_user(operator_id).is_some() {
        return Err(format!("\"{}\" is already enrolled.", operator_id));
    }

    let public_key = public_key.to_lowercase();
    let key_id = sign_utils::key_id_for_public_key(&key_bytes);

    if state.find_key(&key_id).is_some() {
        return Err(format!("The key {} is already tracked.", key_id));
    }

    for pallet in &pallets {
        pallet_utils::validate_pallet_name(pallet)?;
    }

    let now = Utc::now().timestamp();

    // This key becomes the identity root the office pins; the admin's endorsement
    // authorizes it here, the proof-of-possession proves the operator holds it.
    let key = office_utils::endorse_key(&public_key, operator_id, &signing_key_id, pop, now)?;

    state.users.push(UserRecord {
        identifier: operator_id.to_string(),
        enrolled_at: now,
        role,
        pallets,
        identity_root: key_id.clone(),
        class,
        supervisor: supervisor.clone(),
    });
    state.keys.push(key);

    let marked = describe_class(class, supervisor.as_deref());

    let description = format!(
        "Admitted operator \"{}\" as {}{} (identity root {}), by \"{}\".",
        operator_id, role.as_str(), marked, key_id, actor_id
    );
    let parcel = office_utils::stack_office_parcel(&state, &actor, description, &signing_key_id)?;

    output::message("office", format!(
        "Admitted \"{}\" as {}{} with identity root {} (office parcel {}).",
        operator_id, role.as_str(), marked, key_id, parcel
    ));

    Ok(())
}

/// A human-readable suffix describing an automated identity's class and supervisor
/// (empty for a plain human), e.g. " (agent, supervised by alice)".
fn describe_class(class: IdentityClass, supervisor: Option<&str>) -> String {
    if class == IdentityClass::Human {
        return String::new();
    }

    match supervisor {
        Some(supervisor) => format!(" ({}, supervised by {})", class.as_str(), supervisor),
        None => format!(" ({})", class.as_str()),
    }
}

/// Link another device's key to the configured operator's own identity: a sigchain
/// endorsement (§8.5) — the new key is valid because an already-trusted key of the
/// same identity signs for it. Self-service; the office parcel is signed by the
/// endorsing key.
pub fn link(public_key: &str, pop: &str) -> Result<(), String> {
    let actor = config_utils::get_operator()?;
    let (mut state, signing_key_id, actor_id) = require_signing_actor(&actor)?;

    let key_bytes = sign_utils::from_hex(public_key)
        .map_err(|_| "The public key is not valid hex.".to_string())?;

    if key_bytes.len() != 32 {
        return Err("The public key must be 32 bytes (64 hex characters).".to_string());
    }

    let public_key = public_key.to_lowercase();
    let key_id = sign_utils::key_id_for_public_key(&key_bytes);

    if state.find_key(&key_id).is_some() {
        return Err(format!("The key {} is already tracked.", key_id));
    }

    let now = Utc::now().timestamp();

    // The proof-of-possession binds the new key to the operator id, so it only
    // verifies when the other device is configured with the same operator id
    // (or the same profile).
    let key = office_utils::endorse_key(&public_key, &actor_id, &signing_key_id, pop, now)?;

    state.keys.push(key);

    let description = format!(
        "Linked key {} to \"{}\" (endorsed by key {}).",
        key_id, actor_id, signing_key_id
    );
    let parcel = office_utils::stack_office_parcel(&state, &actor, description, &signing_key_id)?;

    output::message("office", format!(
        "Linked key {} to your identity (office parcel {}).", key_id, parcel
    ));

    Ok(())
}

/// Authorize a new key for an already-enrolled operator — the admin-side recovery of
/// §8.6 (an operator who lost every device cannot `link`; an admin's endorsement is
/// scoped to exactly this office, and the proof-of-possession still co-signs, so no
/// one can be attributed a key they do not hold).
pub fn authorize(operator_id: &str, public_key: &str, pop: &str) -> Result<(), String> {
    let actor = config_utils::get_operator()?;
    let (mut state, signing_key_id, actor_id) = require_signing_actor(&actor)?;

    if operator_id == actor_id {
        return Err(
            "That is your own identity; link your own devices with \"office link\" — \
            it endorses the key with your existing key instead of an admin's.".to_string()
        );
    }

    require_admin(&state, &actor_id)?;

    if state.find_user(operator_id).is_none() {
        return Err(format!(
            "\"{}\" is not enrolled; use \"office admit\" for new operators.",
            operator_id
        ));
    }

    let key_bytes = sign_utils::from_hex(public_key)
        .map_err(|_| "The public key is not valid hex.".to_string())?;

    if key_bytes.len() != 32 {
        return Err("The public key must be 32 bytes (64 hex characters).".to_string());
    }

    let public_key = public_key.to_lowercase();
    let key_id = sign_utils::key_id_for_public_key(&key_bytes);

    if state.find_key(&key_id).is_some() {
        return Err(format!("The key {} is already tracked.", key_id));
    }

    let now = Utc::now().timestamp();

    // Cross-identity endorsement: valid because the authorizer is an admin here (the
    // audit enforces the same scope rule on every copy, the remote included).
    let key = office_utils::endorse_key(&public_key, operator_id, &signing_key_id, pop, now)?;

    state.keys.push(key);

    let description = format!(
        "Authorized key {} for \"{}\", by admin \"{}\".",
        key_id, operator_id, actor_id
    );
    let parcel = office_utils::stack_office_parcel(&state, &actor, description, &signing_key_id)?;

    output::message("office", format!(
        "Authorized key {} for \"{}\" (office parcel {}).",
        key_id, operator_id, parcel
    ));

    Ok(())
}

/// Change a user's role (and a writer's pallet grants). An admin privilege, with
/// lockout protection: the office must always retain at least one admin.
pub fn role(identifier: &str, role: &str, pallets: Vec<String>) -> Result<(), String> {
    let actor = config_utils::get_operator()?;
    let (mut state, signing_key_id, actor_id) = require_signing_actor(&actor)?;
    require_admin(&state, &actor_id)?;

    let role = Role::parse(role)?;

    for pallet in &pallets {
        pallet_utils::validate_pallet_name(pallet)?;
    }

    if state.find_user(identifier).is_none() {
        return Err(format!("\"{}\" is not enrolled.", identifier));
    }

    let remaining_admins = state.users.iter()
        .filter(|user| user.role == Role::Admin && user.identifier != identifier)
        .count();

    if role != Role::Admin && remaining_admins == 0 {
        return Err(
            "This would leave the office without an admin; no one could manage users \
            or keys anymore. Promote another admin first.".to_string()
        );
    }

    for user in state.users.iter_mut() {
        if user.identifier == identifier {
            user.role = role;
            user.pallets = pallets.clone();
        }
    }

    let description = format!(
        "Changed the role of \"{}\" to {}, by \"{}\".",
        identifier, role.as_str(), actor_id
    );
    let parcel = office_utils::stack_office_parcel(&state, &actor, description, &signing_key_id)?;

    output::message("office", format!(
        "\"{}\" is now a(n) {} (office parcel {}).", identifier, role.as_str(), parcel
    ));

    Ok(())
}

/// Rotate the configured operator's keys: issue a fresh one and retire the old active
/// ones. The office parcel is signed with the *old* key — that is what proves the
/// rotation was authorized by the previous key's owner. Each retirement carries a
/// distrust boundary (the pallet heads vouched for right now, the remote's included),
/// so the old keys cannot silently sign anything new.
pub async fn rotate(offline: bool, passphrase: bool) -> Result<(), String> {
    let actor = config_utils::get_operator()?;
    let (mut state, old_key_id, actor_id) = require_signing_actor(&actor)?;

    let boundary = revocation_boundary(offline).await?;

    let now = Utc::now().timestamp();
    let (new_key_id, public_key) = generate_operator_key(&actor_id, passphrase)?;

    // The sigchain endorsement: the old key authorizes the new one (both halves are
    // on this machine), and the new key proves possession of itself.
    let pop = office_utils::sign_key_pop(&new_key_id, &public_key, &actor_id)?;
    let new_key = office_utils::endorse_key(&public_key, &actor_id, &old_key_id, &pop, now)?;

    for key in state.keys.iter_mut() {
        if key.operator == actor_id && key.is_active() {
            key.retired_at = Some(now);
            key.revocation_reason = Some(RevocationReason::Retirement);
            key.distrust_boundary = boundary.clone();
        }
    }

    state.keys.push(new_key);

    let description = format!(
        "Rotated the keys of \"{}\": {} replaces {}.",
        actor_id, new_key_id, old_key_id
    );
    let parcel = office_utils::stack_office_parcel(&state, &actor, description, &old_key_id)?;

    output::message("office", format!(
        "Rotated keys (office parcel {}).\nNew key id: {}", parcel, new_key_id
    ));

    Ok(())
}

/// Retire a key (a decommissioned machine, or — with --compromised — one that may be
/// in someone else's hands). The revocation records a reason and a distrust boundary:
/// the pallet heads vouched for right now (the remote's included). Signatures by the
/// key outside that ancestry fail every future audit — exact ancestry, never time, so
/// a shifted clock changes nothing.
pub async fn retire(key_id: &str, compromised: bool, offline: bool) -> Result<(), String> {
    let actor = config_utils::get_operator()?;
    let (mut state, signing_key_id, actor_id) = require_signing_actor(&actor)?;

    let Some(target) = state.find_key(key_id) else {
        return Err(format!("No key {} is tracked.", key_id));
    };

    if !target.is_active() {
        return Err(format!("The key {} is already retired.", key_id));
    }

    let is_own = target.operator == actor_id;

    // Key management is self-service; touching someone else's key is an admin move.
    if !is_own {
        require_admin(&state, &actor_id)?;
    }

    let own_active_count = state.active_keys_of(&actor_id).len();

    if is_own && own_active_count == 1 {
        return Err(
            "This is your only active key; retiring it would lock you out. Use \
            \"office rotate\" instead.".to_string()
        );
    }

    let target_operator = target.operator.clone();
    let boundary = revocation_boundary(offline).await?;
    let now = Utc::now().timestamp();

    let reason = if compromised {
        RevocationReason::Compromise
    } else {
        RevocationReason::Retirement
    };

    for key in state.keys.iter_mut() {
        if key.key_id == key_id {
            key.retired_at = Some(now);
            key.revocation_reason = Some(reason);
            key.distrust_boundary = boundary.clone();
        }
    }

    // Never sign a retirement with the key being retired (it may be compromised).
    let signing_key_id = if signing_key_id == key_id {
        state.signing_key_of(&actor_id)
            .map(|key| key.key_id.clone())
            .ok_or("No other active key of yours is present on this machine to sign with.".to_string())?
    } else {
        signing_key_id
    };

    let description = format!(
        "Revoked key {} of \"{}\" ({}), by \"{}\".",
        key_id, target_operator, reason.as_str(), actor_id
    );
    let parcel = office_utils::stack_office_parcel(&state, &actor, description, &signing_key_id)?;

    let mut report = format!("Revoked key {} ({}; office parcel {}).", key_id, reason.as_str(), parcel);

    if compromised {
        report.push_str(
            "\nAnything this key signed beyond the current pallet heads will fail audit \
            from now on; review what sits inside the boundary too — the compromise may \
            predate this revocation."
        );
    }

    output::message("office", report);

    Ok(())
}

/// The distrust boundary of a revocation: every pallet head vouched for at this
/// moment. When a remote is configured its heads join the boundary (like enroll's
/// trust boundary): work the revoked key legitimately lifted must not fall outside.
async fn revocation_boundary(offline: bool) -> Result<Vec<String>, String> {
    let mut boundary: Vec<String> = Vec::new();

    for pallet in pallet_utils::list_pallets()? {
        if let Some(head) = pallet_utils::get_pallet_head(&pallet)? {
            boundary.push(head);
        }
    }

    let has_remote =
        config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?.is_some();

    if has_remote && !offline {
        let client = RemoteClient::from_config()?;

        let info = client.fetch_info().await.map_err(|e| format!(
            "{}\nA revocation's distrust boundary must vouch for work already on the \
            remote, so it must be reachable. Retry when it is, or pass --offline — \
            knowing that parcels signed by the revoked key that only the remote has \
            would fall outside the boundary and fail audit.",
            e
        ))?;

        for head in info.pallets.values() {
            if !boundary.contains(head) {
                boundary.push(head.clone());
            }
        }
    }

    Ok(boundary)
}

/// List the users and their keys.
pub async fn list() -> Result<(), String> {
    let state = office_utils::read_office_state()?;

    if state.users.is_empty() {
        output::emit("office", &OfficeListing { enrolled: false, users: Vec::new() });

        return Ok(());
    }

    // Display names resolved through the configured remote (server-mediated, §8.12) —
    // best-effort; the pseudonymous identifiers stand alone otherwise.
    let names = remote_utils::resolve_office_display_names().await;

    let users = state.users.iter().map(|user| {
        let keys = state.keys.iter()
            .filter(|key| key.operator == user.identifier)
            .map(|key| OfficeKey {
                key_id: key.key_id.clone(),
                retired: key.retired_at.is_some(),
                on_this_machine: key.retired_at.is_none() && sign_utils::has_private_key(&key.key_id),
                protected: sign_utils::is_key_encrypted(&key.key_id),
                identity_root: user.identity_root == key.key_id,
            })
            .collect();

        OfficeUser {
            identifier: user.identifier.clone(),
            name: names.get(&user.identifier).cloned(),
            role: user.role.as_str().to_string(),
            class: user.class.as_str().to_string(),
            supervisor: user.supervisor.clone(),
            pallets: user.pallets.clone(),
            keys,
        }
    }).collect();

    output::emit("office", &OfficeListing { enrolled: true, users });

    Ok(())
}

/// The office roster: users, their roles/grants, and their keys.
#[derive(Serialize)]
struct OfficeListing {
    /// Whether trust is established (anyone is enrolled).
    enrolled: bool,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    users: Vec<OfficeUser>,
}

/// One enrolled operator.
#[derive(Serialize)]
struct OfficeUser {
    identifier: String,

    /// The resolved display name, when a resolution hook supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,

    role: String,

    /// The identity class (§7.1): human / agent / bot / service.
    class: String,

    /// The supervising human, for an automated identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor: Option<String>,

    /// The pallets a writer is restricted to (empty = all pallets).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pallets: Vec<String>,

    keys: Vec<OfficeKey>,
}

/// One key of an operator.
#[derive(Serialize)]
struct OfficeKey {
    key_id: String,
    retired: bool,

    /// Whether the private half is present on this machine (an active key you can sign with).
    on_this_machine: bool,

    /// Whether the local private key is passphrase-protected (encrypted at rest).
    protected: bool,

    /// Whether this is the operator's pinned identity root.
    identity_root: bool,
}

impl CommandOutput for OfficeListing {
    fn render_human(&self) {
        if !self.enrolled {
            println!(
                "No one is enrolled. Establish trust with \"office enroll\" (this makes \
                signing mandatory and cannot be undone)."
            );

            return;
        }

        for user in &self.users {
            let grants = if user.role == "writer" && !user.pallets.is_empty() {
                format!(" of {}", user.pallets.join(", "))
            } else {
                String::new()
            };

            let display = match &user.name {
                Some(name) => format!("{} ({})", user.identifier, name),
                None => user.identifier.clone(),
            };

            // Automated identities carry a class (and, for agents, a supervisor).
            let class = if user.class == "human" {
                String::new()
            } else {
                match &user.supervisor {
                    Some(supervisor) => format!(" [{}, supervised by {}]", user.class, supervisor),
                    None => format!(" [{}]", user.class),
                }
            };

            println!("{} — {}{}{}", display, user.role, grants, class);

            for key in &user.keys {
                let status = if key.retired {
                    "retired"
                } else if key.on_this_machine {
                    "active, on this machine"
                } else {
                    "active"
                };

                let protected = if key.on_this_machine && key.protected { ", protected" } else { "" };
                let root = if key.identity_root { ", identity root" } else { "" };

                println!("  key {} ({}{}{})", key.key_id, status, protected, root);
            }
        }
    }
}

/// Re-genesis (§8.7): the recovery primitive for a fully locked chain. Creates a new
/// self-endorsed trust root for the configured operator, pins the old office head as
/// attested history, and replaces the trust anchor. It does not create trust from
/// nothing — the authority is whoever controls this warehouse's files (standalone) or
/// the server operator (remote; the static token gates the reset there). LOUD by
/// design: every other holder of the old anchor must consciously re-accept.
pub fn regenesis(confirm: bool) -> Result<(), String> {
    let operator = config_utils::get_operator()?;

    let Some(old_anchor) = office_utils::read_trust_anchor()? else {
        return Err(
            "Trust is not established for this warehouse; there is nothing to reset. \
            Use \"office enroll\".".to_string()
        );
    };

    let Some(old_office_head) = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)? else {
        return Err("Trust is established but the office pallet is missing.".to_string());
    };

    // A working admin has every self-service and admin tool available; a trust reset
    // is strictly for the chain nobody can extend anymore.
    let state = office_utils::read_office_state()?;

    if let Some(user) = state.find_user(&operator.identifier) {
        if user.role == Role::Admin && state.signing_key_of(&operator.identifier).is_some() {
            return Err(
                "You are an admin with a usable key: manage the office with \"rotate\", \
                \"link\", \"admit\" and \"retire\". A re-genesis is the recovery for a \
                chain nobody can extend, and it forces every holder of the current \
                anchor to re-accept trust.".to_string()
            );
        }
    }

    if !confirm {
        // Dry-run guidance is for a person; under --json the refusal below is the result.
        crate::human!("Re-genesis would RESET this warehouse's trust:");
        crate::human!("  current genesis:  {}", old_anchor.genesis);
        crate::human!("  office head:      {} (pinned as attested history)", old_office_head);
        crate::human!();
        crate::human!("A new trust root is created for operator \"{}\". Prior history stays", operator.identifier);
        crate::human!("readable but degrades from verified to attested; all enrolled operators");
        crate::human!("and keys are gone from the new office; every clone of this warehouse will");
        crate::human!("refuse to sync until its holder consciously re-accepts the new anchor.");
        crate::human!();
        crate::human!("Re-run with --confirm to proceed.");

        return Err("Re-genesis needs --confirm.".to_string());
    }

    // The new boundary: everything that exists right now — across both namespaces, so
    // the old office chain included — becomes the attested history of the new anchor.
    let mut boundary: Vec<String> = Vec::new();

    for (_, head) in pallet_utils::all_pallet_refs()? {
        boundary.push(head);
    }

    let now = Utc::now().timestamp();
    let (key_id, public_key) = sign_utils::generate_keypair(&operator.identifier)?;
    let pop = office_utils::sign_key_pop(&key_id, &public_key, &operator.identifier)?;
    let root_key = office_utils::endorse_key(&public_key, &operator.identifier, &key_id, &pop, now)?;

    let new_state = OfficeState {
        users: vec![UserRecord {
            identifier: operator.identifier.clone(),
            enrolled_at: now,
            role: Role::Admin,
            pallets: Vec::new(),
            identity_root: key_id.clone(),
            // Re-genesis re-roots the chain in the recovering person.
            class: IdentityClass::Human,
            supervisor: None,
        }],
        keys: vec![root_key],
    };

    let description = format!(
        "Re-genesis: new trust root for operator \"{}\" (adopts office head {}).",
        operator.identifier, old_office_head
    );
    let genesis = office_utils::stack_office_genesis(&new_state, &operator, description, &key_id)?;

    office_utils::replace_trust_anchor(&office_utils::TrustAnchor {
        genesis: genesis.clone(),
        enabled_at: now,
        boundary,
        prior_genesis: Some(old_anchor.genesis.clone()),
        adopts: Some(old_office_head.clone()),
    })?;

    output::message("office", format!(
        "TRUST RESET — re-genesis complete.\n\
        New genesis parcel: {}\n\
        Identity root key:  {}\n\
        Prior genesis:      {} (chain of custody, recorded in the anchor)\n\
        Adopted history:    {} (frozen, attested by the new anchor)\n\n\
        Every clone must consciously re-accept the new anchor with \
        \"office accept-regenesis\". A remote accepts the reset only from the server \
        operator's static token.",
        genesis, key_id, old_anchor.genesis, old_office_head
    ));

    Ok(())
}

/// Consciously accept a remote's re-genesis: the deliberate counterpart of the
/// refusal every sync raises after a trust reset (the SSH host-key-change moment).
pub async fn accept_regenesis(confirm: bool) -> Result<(), String> {
    let client = RemoteClient::from_config()?;

    if !confirm {
        let Some(local) = office_utils::read_trust_anchor()? else {
            return Err(
                "This warehouse has no trust anchor; a plain \"lower\" adopts the \
                remote's trust on first contact.".to_string()
            );
        };

        let info = client.fetch_info().await?;

        let Some(remote_trust) = &info.trust else {
            return Err("The remote has no trust anchor; there is no re-genesis to accept.".to_string());
        };

        if remote_trust.genesis == local.genesis {
            output::message("office", "The remote's trust anchor matches this warehouse's; nothing to accept.");

            return Ok(());
        }

        crate::human!("The remote's trust anchor was RESET (re-genesis):");
        crate::human!("  your pinned genesis: {}", local.genesis);
        crate::human!("  remote genesis:      {}", remote_trust.genesis);
        crate::human!("  names as its prior:  {}", remote_trust.prior_genesis.as_deref().unwrap_or("(none)"));
        crate::human!("  adopts office head:  {}", remote_trust.adopts.as_deref().unwrap_or("(none)"));
        crate::human!();
        crate::human!("Accepting means trusting whoever performed the reset as the new root of");
        crate::human!("this warehouse. Verify that out-of-band (ask the operators you know), then");
        crate::human!("re-run with --confirm.");

        return Err("Accepting a re-genesis needs --confirm.".to_string());
    }

    let (old, new) = remote_utils::accept_regenesis(&client).await?;

    output::message("office", format!(
        "Accepted the re-genesis.\n\
        Old genesis: {}\n\
        New genesis: {}\n\
        The prior history is retained as attested; \"lower\" to continue.",
        old.genesis, new.genesis
    ));

    Ok(())
}

/// Generate a keypair for an operator, protecting it with a passphrase when asked
/// (the human-vs-agent boundary — a protected key cannot be used non-interactively).
/// The passphrase is prompted and confirmed by the head (`passphrase::prompt_new`).
///
/// # Arguments
/// * `owner`      - The operator id the key belongs to.
/// * `passphrase` - Whether to protect the key with a passphrase.
///
/// # Returns
/// * `Ok((String, String))` - The key id and public key (hex).
/// * `Err(String)`          - If the passphrase could not be read, or the key stored.
fn generate_operator_key(owner: &str, passphrase: bool) -> Result<(String, String), String> {
    if passphrase {
        let passphrase = crate::passphrase::prompt_new()?;
        sign_utils::generate_keypair_encrypted(owner, &passphrase)
    } else {
        sign_utils::generate_keypair(owner)
    }
}

/// Require that the configured operator is enrolled and can sign right now: trust is
/// established and one of their active keys is present on this machine.
///
/// # Returns
/// * `Ok((OfficeState, String, String))` - The current office state, the signing key
///                                         id, and the actor's operator id.
/// * `Err(String)`                       - If trust is missing, or the operator
///                                         cannot sign.
pub(crate) fn require_signing_actor(actor: &Operator) -> Result<(OfficeState, String, String), String> {
    if office_utils::read_trust_anchor()?.is_none() {
        return Err(
            "Trust is not established for this warehouse yet; use \"office enroll\" first.".to_string()
        );
    }

    let state = office_utils::read_office_state()?;

    let Some(actor_id) = state.find_user(&actor.identifier).map(|user| user.identifier.clone()) else {
        return Err(format!(
            "\"{}\" is not enrolled; ask an enrolled operator to admit you.",
            actor.identifier
        ));
    };

    let signing_key_id = state.signing_key_of(&actor_id)
        .map(|key| key.key_id.clone())
        .ok_or(format!(
            "No active key of \"{}\" is present on this machine.",
            actor_id
        ))?;

    Ok((state, signing_key_id, actor_id))
}

/// Require that the operator is an office admin (admissions, roles, others' keys).
/// The remote enforces the same rule on every office lift, so this is the early,
/// friendly version of a refusal that would otherwise arrive at lift time.
fn require_admin(state: &OfficeState, actor_id: &str) -> Result<(), String> {
    let is_admin = state.find_user(actor_id)
        .map(|user| user.role == Role::Admin)
        .unwrap_or(false);

    if is_admin {
        Ok(())
    } else {
        Err(format!(
            "\"{}\" is not an office admin; only admins manage users, roles and \
            others' keys.",
            actor_id
        ))
    }
}
