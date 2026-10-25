//! Executed evidence for the **collected-referent class**: a durable hash pin whose referent has
//! been garbage-collected.
//!
//! `gc_utils::collect_live_set` roots pallet refs, bay-scoped parcels, every bay's staged
//! inventory (FORK-156) and every trust-pin root (`office_utils::collect_trust_pin_roots`: the
//! anchor's `adopts` pin, its `boundary` snapshot, and every key's revocation
//! `distrust_boundary`). Several other places
//! still write a parcel or object hash durably with no such root, and read it back regardless.
//! When gc collects an unrooted referent, each reader fails differently — and the members
//! disagree about what correct behaviour even is, which is why the pallet-lifecycle design
//! replaced its class invariant with a per-member ledger.
//!
//! | member | reader kind | leg here |
//! | --- | --- | --- |
//! | revocation `distrust_boundary` | verifying | **FIXED (FORK-81)** — rooted; the two `a_boundary_pin_*`/`a_distrust_boundary_pin_*` legs (FORK-63/64 spike) |
//! | `audit`'s boundary head | verifying | **FIXED (FORK-81)** — rooted; `an_undone_boundary_head_*`, `a_boundary_head_and_the_legacy_parcel_*`. The residual gap a *rooted* pin can still reach — never fetched, or genuinely lost, since gc can no longer produce it — is a separate finding with its own legs (not gc-driven, no canary): `an_absent_interior_ancestor_behind_a_present_boundary_head_is_named_not_accused_of_tampering`, `a_genuinely_pre_trust_side_pallet_parcel_behind_an_absent_boundary_head_refuses_instead_of_accusing`, `the_accusation_still_fires_with_no_gap_at_all`. **PR #121 round 1 narrowed the tolerance to interior-only gaps; round 2 reverted that narrowing** — an absent boundary HEAD, exactly like an absent interior ancestor, un-resolves the boundary walk's negative for anything the present portion cannot itself settle, because there is no such thing as an "unrelated" absent reference: relatedness to the parcel under test is precisely the question the walk exists to answer (see `audit_utils::verify_pallet_history`'s `boundary_gap` doc). Reproduced end to end against a real origin server, from ordinary commands, in `tests/remote.rs`'s two Construction T/D legs. |
//! | `CherryPickState.source` | **acting** | **FIXED (FORK-82)** — rooted while the pick is in progress; `a_cherry_pick_in_progress_roots_its_source_*`. `undo` refuses to put back a completed pick whose source was collected since: `undoing_a_completed_pick_whose_source_was_collected_*`. A source lost anyway is named with its abort: `a_pick_whose_source_was_lost_*` |
//! | `Tag.subject` under a taint | **healing** | **FIXED (FORK-83)** — a tag's own reference never counts, an absent subject is noted; `a_torn_taint_over_an_absent_tag_subject_heals_*`, `a_recorded_taint_over_a_collected_tag_subject_clears`, and `a_lost_tagged_parcel_that_is_also_an_ancestor_*` for the other direction |
//! | staged inventory shard | **healing**, then acting | **FIXED (FORK-156)** — rooted; `staged_content_survives_collection_*`, `a_full_repack_keeps_staged_content_*`. Found as FORK-83's widened half: unrooted, collection deleted staged work, a torn taint then wedged the store, and a full repack lost it silently |
//!
//! `Tag.subject`'s *rendering* reader is pinned separately in `tag_subject_gc.rs` (FORK-79).
//!
//! **Five legs carry a canary** — a parcel pinned by nothing — and assert it was collected, so
//! "the pin survived" can never be satisfied by a collector that swept nothing. The canary check
//! holds both before and after any fix; it is the legs' own assertions that invert. (**Ten** of
//! the eleven legs drive collection — eight call `collect_garbage`, and
//! `an_undone_boundary_head_survives_the_sweep_and_audit_passes` and
//! `a_full_repack_keeps_staged_content_and_the_next_stack_restores_it` drive the same live set
//! through `compact --all` — so "gc-driven" is not the dividing line. Only the torn-taint control
//! collects nothing.)
//!
//! The other six discriminate differently: the two anchor legs now assert `b_present &&
//! legacy_present && audit succeeds` (FORK-81 rooted the boundary pin, so both survive together;
//! before the fix this was `!b_present && legacy_present`, already two-sided in the other
//! direction); `a_torn_taint_alone_...` is a matched companion to the FORK-83 legs rather than a
//! canary user; `staged_content_survives_...` names the exact blob it staged and asserts that
//! specific object survived collection; `a_full_repack_keeps_...` asserts the staged content
//! itself comes back through `restore`; and `a_recorded_taint_over_a_collected_tag_subject_clears`
//! asserts directly that the tag subject it heals over was collected.
//!
//! **Five further legs sit outside this counting entirely** — three added for FORK-81's second
//! finding, FORK-83's `a_lost_tagged_parcel_that_is_also_an_ancestor_still_refuses`, and FORK-82's
//! `a_pick_whose_source_was_lost_refuses_naming_the_pick_and_the_abort_works`. They
//! delete an object directly rather than driving `collect_garbage` — the state they construct (a rooted pin's referent simply never held locally, or genuinely lost) is not
//! something gc can produce any more, so there is no sweep for a canary to witness and none is
//! used. See
//! `an_absent_interior_ancestor_behind_a_present_boundary_head_is_named_not_accused_of_tampering`,
//! `a_genuinely_pre_trust_side_pallet_parcel_behind_an_absent_boundary_head_refuses_instead_of_accusing`,
//! `the_accusation_still_fires_with_no_gap_at_all`,
//! `a_lost_tagged_parcel_that_is_also_an_ancestor_still_refuses` and
//! `a_pick_whose_source_was_lost_refuses_naming_the_pick_and_the_abort_works`. FORK-83's
//! `a_torn_taint_heals_over_a_malformed_tag_record_and_notes_it` sits outside it too: it writes a
//! malformed tag record directly and collects nothing.

use std::path::PathBuf;
use std::process::Output;

use forklift_core::globals::StorageRootScope;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::util::{audit_utils, file_utils, gc_utils, object_utils, office_utils};

mod common;
use common::forklift_command;

struct Warehouse {
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let warehouse = Warehouse::new_unenrolled(name);
        warehouse.run_ok(&["office", "enroll"]);

        warehouse
    }

    /// A prepared, configured warehouse with **no trust established**, so parcels stacked on it
    /// are unsigned and become "legacy" once an anchor is written. The anchor-boundary leg needs
    /// pre-trust history; every other leg wants `new`.
    fn new_unenrolled(name: &str) -> Warehouse {
        let base =
            std::env::temp_dir().join(format!("forklift-boundary-gc-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "operator.identifier", "spike@forklift"]);

        warehouse
    }

    fn run(&self, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`{}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn stack(&self, file: &str, content: &str, message: &str) -> String {
        let path = self.root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        self.run_ok(&["load", "."]);
        self.run_ok(&["stack", message]);

        self.head(&self.current_pallet())
    }

    fn current_pallet(&self) -> String {
        // The bay position record: a header line, then the pallet name, then the base.
        std::fs::read_to_string(self.root.join(".forklift").join("position"))
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .to_string()
    }

    fn head(&self, pallet: &str) -> String {
        std::fs::read_to_string(self.root.join(".forklift").join("pallets").join(pallet))
            .unwrap()
            .trim()
            .to_string()
    }

    /// What a pallet-deletion verb would do: unlink the ref file.
    fn delete_pallet_ref(&self, pallet: &str) {
        std::fs::remove_file(self.root.join(".forklift").join("pallets").join(pallet))
            .expect("the ref existed");
    }

    /// Every loose object hash currently in the store, as `<dir><file>`. Used to name the exact
    /// object a fixture created, so a leg can assert *which* hash a refusal is about rather than
    /// that some hash is.
    ///
    /// **Only the two-hex fan-out directories.** `.forklift/objects` also holds `pack/`
    /// (`pack_utils.rs:45`), so an unfiltered walk returns pack filenames alongside hashes — which
    /// would silently bind a caller's "the object I just created" to `packab12cd.pack` after any
    /// `compact`. No current caller runs one, but this is a `Warehouse` method and a sibling leg
    /// does.
    fn loose_objects(&self) -> std::collections::BTreeSet<String> {
        let mut found = std::collections::BTreeSet::new();
        let objects = self.root.join(".forklift").join("objects");

        let Ok(fan_out) = std::fs::read_dir(&objects) else { return found; };

        for prefix in fan_out.flatten() {
            let head = prefix.file_name().to_string_lossy().to_string();

            if head.len() != 2 || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }

            let Ok(entries) = std::fs::read_dir(prefix.path()) else { continue; };

            for object in entries.flatten() {
                found.insert(format!("{}{}", head, object.file_name().to_string_lossy()));
            }
        }

        found
    }

    /// Delete a loose object's file directly — simulating either cause behind a trust-pin gap
    /// now that FORK-81 has rooted `anchor.boundary`/`distrust_boundary` as GC roots: a head this
    /// store never fetched to begin with (`office enroll` pins a remote's *declared* heads by
    /// hash without fetching them, and `franchise` copies the anchor verbatim while fetching
    /// only the one pallet it franchises), or genuine object loss. `gc` can no longer produce
    /// this state for a rooted pin (the legs above prove the opposite — a rooted pin survives),
    /// so a leg that needs it reaches for this instead.
    fn delete_object(&self, hash: &str) {
        let path = self.root.join(".forklift").join("objects").join(&hash[..2]).join(&hash[2..]);
        std::fs::remove_file(&path).unwrap_or_else(|e| {
            panic!("failed to delete object {} at {}: {}", hash, path.display(), e)
        });
    }

    /// Strip a parcel's signature sidecar directly, forcing it to classify as unsigned even
    /// though it was actually signed at `stack` time. Used to construct a parcel that is
    /// genuinely outside the trust boundary: no shipped command can produce an unsigned,
    /// out-of-boundary parcel once trust is established (that refusal is the whole point of
    /// enrolling), so this stands in for whatever real corruption or forgery the code's actual
    /// tampering case exists to catch.
    fn delete_signature(&self, hash: &str) {
        let path = self.root.join(".forklift").join("objects").join(&hash[..2])
            .join(format!("{}.sig", &hash[2..]));
        std::fs::remove_file(&path).unwrap_or_else(|e| {
            panic!("failed to delete signature for {} at {}: {}", hash, path.display(), e)
        });
    }

    fn scoped<T>(&self, work: impl FnOnce() -> T) -> T {
        let _scope = StorageRootScope::enter(&self.root);

        work()
    }
}

/// The fixture both legs share.
///
/// Two pallets are built and one is thrown away *before* the revocation, so the fixture carries
/// a **canary**: a parcel that is genuinely unreachable and pinned by nothing. Every leg asserts
/// the canary was collected, which is what proves the collector actually swept — without it,
/// "the pin is still present" is satisfied just as well by a collector that did nothing at all,
/// and the inverted post-fix assertion would pass against an early-bailing `collect_live_set`.
///
/// Returns the retired key, the pinned `side` head, and the canary hash.
fn warehouse_with_a_pinned_side_head(name: &str)
    -> (Warehouse, office_utils::KeyRecord, String, String) {
    let warehouse = Warehouse::new(name);

    warehouse.stack("app.txt", "v1\n", "first");

    // The canary: built, then orphaned before the revocation snapshot, so no boundary names it.
    warehouse.run_ok(&["palletize", "canary"]);
    let canary = warehouse.stack("canary.txt", "c1\n", "canary work");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.delete_pallet_ref("canary");

    warehouse.run_ok(&["palletize", "side"]);
    let side_head = warehouse.stack("side.txt", "s1\n", "side work");
    warehouse.run_ok(&["shift", "main"]);

    // A revocation pins every local pallet head as its distrust boundary.
    warehouse.run_ok(&["office", "rotate", "--offline"]);

    let retired_key = warehouse.scoped(|| {
        office_utils::read_office_state()
            .unwrap()
            .keys
            .iter()
            .find(|key| !key.distrust_boundary.is_empty())
            .expect("the rotation retired a key with a boundary")
            .clone()
    });

    assert!(
        retired_key.distrust_boundary.contains(&side_head),
        "the side head must be pinned: {:?}",
        retired_key.distrust_boundary
    );

    // The canary must NOT be pinned, or it could not witness a sweep.
    assert!(
        !retired_key.distrust_boundary.contains(&canary),
        "the canary must be unpinned: {:?}",
        retired_key.distrust_boundary
    );

    // The pin is present and the boundary resolves before anything is collected — the control
    // that keeps the deletion leg below from being red for an unrelated reason. The canary is
    // present too, so its later absence is attributable to the sweep.
    warehouse.scoped(|| {
        assert!(file_utils::does_object_exist(&side_head).unwrap(), "the pin is present");
        assert!(file_utils::does_object_exist(&canary).unwrap(), "the canary is present");

        let mut memo = audit_utils::DistrustBoundaryMemo::new();
        assert!(memo.resolvable(&retired_key).unwrap(), "the boundary resolves to begin with");
    });

    (warehouse, retired_key, side_head, canary)
}

/// Assert the sweep actually ran: the unpinned, unreachable canary is gone. Every leg calls
/// this, before and after any fix — it is what stops "the pin survived" from being satisfiable
/// by a collector that deleted nothing.
fn assert_the_sweep_ran(warehouse: &Warehouse, canary: &str, deleted: usize) {
    let canary_present = warehouse.scoped(|| file_utils::does_object_exist(canary).unwrap());

    assert!(
        !canary_present,
        "the collector did not sweep: the unpinned, unreachable canary {} survived",
        canary
    );
    assert!(deleted > 0, "the collector reported deleting nothing");
}

/// Read the pin's fate after a collection.
fn present_and_resolvable(warehouse: &Warehouse,
                          key: &office_utils::KeyRecord,
                          head: &str) -> (bool, bool) {
    warehouse.scoped(|| {
        let present = file_utils::does_object_exist(head).unwrap();

        let mut memo = audit_utils::DistrustBoundaryMemo::new();
        let resolvable = memo.resolvable(key).unwrap();

        (present, resolvable)
    })
}

/// CONTROL. With the ref in place, `gc` keeps the pin while still sweeping the canary — so the
/// collector is neither indiscriminate nor inert, and the deletion leg's result below is
/// attributable to the deletion and to nothing else.
#[test]
fn a_boundary_pin_survives_gc_while_its_pallet_ref_exists() {
    let (warehouse, key, side_head, canary) = warehouse_with_a_pinned_side_head("control");

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    let (present, resolvable) = present_and_resolvable(&warehouse, &key, &side_head);

    println!(
        "CONTROL: gc deleted {} object(s); pin present = {}; boundary resolvable = {}",
        stats.deleted, present, resolvable
    );

    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(present, "the pin must survive gc while its ref exists");
    assert!(resolvable, "the boundary must still resolve while its ref exists");
}

/// THE FIX (FORK-81). Unlink the ref — exactly what a pallet-deletion verb would do — and
/// `gc` now keeps a hash that a signed revocation still names: `office_utils::
/// collect_trust_pin_roots` roots every key's `distrust_boundary`, so the parcels that
/// exculpated the revoked key's signatures stay reachable regardless of what happens to any
/// pallet ref.
///
/// This was `a_boundary_pin_is_collected_once_its_pallet_ref_is_deleted`, and it was green
/// pinning the DEFECT: the same two assertions read `!present`/`!resolvable`. Reverting the
/// production fix (rooting `distrust_boundary` in `collect_live_set`) reddens both `assert!`s
/// below — `present` first (the pin itself no longer survives), so `resolvable` never even
/// gets checked against a live boundary. The canary check does NOT invert — it holds in both
/// worlds, which is the point of it.
#[test]
fn a_distrust_boundary_pin_survives_its_pallet_ref_being_deleted() {
    let (warehouse, key, side_head, canary) = warehouse_with_a_pinned_side_head("deleted");

    warehouse.delete_pallet_ref("side");

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    let (present, resolvable) = present_and_resolvable(&warehouse, &key, &side_head);

    println!(
        "FIXED: gc deleted {} object(s); pin present = {}; boundary resolvable = {}",
        stats.deleted, present, resolvable
    );

    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(present, "the pin must survive gc even once its pallet ref is deleted");
    assert!(resolvable, "and the distrust boundary must still resolve");
}

/// THE FIX (FORK-81): does an undone boundary head survive `compact --all`, and does `audit`
/// then pass with the pre-trust parcel counted as legacy?
///
/// Before the fix, `undo` moving a head *backwards* (`journal_utils.rs:191`) orphaned the
/// boundary head with no ref-deletion verb at all, and `compact --all` then dropped it once it
/// had been packed — which left `audit` refusing with a tampering accusation, since the boundary
/// head it needed to resolve the pre-trust legacy tally was gone. Now `anchor.boundary` is itself
/// a GC root (`office_utils::collect_trust_pin_roots`), so the same repack keeps it, and the
/// boundary always resolves.
///
/// This was `the_false_tampering_state_is_reachable_with_shipped_commands_only`, and it was
/// green pinning the DEFECT (the boundary head reclaimed, `audit` refusing with a tampering
/// accusation). Reverting the production fix reddens `b_present` first — the boundary head is
/// collected again — so `audit`'s own assertion never gets exercised against a live boundary.
///
/// No `delete_pallet_ref` here: every step is a CLI command a user runs.
#[test]
fn an_undone_boundary_head_survives_the_sweep_and_audit_passes() {
    let warehouse = Warehouse::new_unenrolled("live");

    let legacy = warehouse.stack("app.txt", "v1\n", "legacy one");
    let b = warehouse.stack("app.txt", "v2\n", "legacy two");

    warehouse.run_ok(&["office", "enroll"]);

    let boundary = warehouse.scoped(|| {
        office_utils::read_trust_anchor().unwrap().expect("an anchor").boundary
    });
    assert_eq!(boundary, vec![b.clone()], "the boundary must be exactly [b]");

    // Pack. NOT vestigial: `compact --all` below only ever reclaims already-PACKED garbage, never
    // loose objects (`pack_utils.rs:1678-1679`) — so without this step `b` would still be loose
    // when `compact --all` runs, which would leave it present regardless of whether `b` is
    // actually rooted. This step is what makes the leg a falsifier at all: it is what makes `b`'s
    // survival below actually depend on the fix under test, rather than passing vacuously either
    // way (stuck-green).
    warehouse.run_ok(&["compact"]);

    // Soft undo moves `main` back off `b`, orphaning it (no pallet ref reaches `b` any more).
    // No second ref is needed: `main` itself lands on `legacy`, keeping it alive.
    //
    // ORDERING MATTERS, and getting it wrong is what made a first attempt at this leg pass
    // spuriously: `shift` is journaled too (`cli.rs:1639`), so a `shift` between the stack and the
    // `undo` makes `undo` revert the *shift* and leave `main` still on `b`. The stack must be the
    // newest journal entry.
    warehouse.run_ok(&["undo"]);

    // The shipped client-side collector.
    warehouse.run_ok(&["compact", "--all"]);

    let (b_present, legacy_present) = warehouse.scoped(|| {
        (
            file_utils::does_object_exist(&b).unwrap(),
            file_utils::does_object_exist(&legacy).unwrap(),
        )
    });

    let audit = warehouse.run(&["audit"]);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );

    println!(
        "FIXED: b present = {}; legacy present = {}; audit exit = {:?}\n{}",
        b_present, legacy_present, audit.status.code(), out.trim()
    );

    assert!(b_present, "the boundary head must survive `compact --all` now that it is rooted");
    assert!(legacy_present, "the legacy parcel must survive as `main`'s head");
    assert!(
        audit.status.success(),
        "audit must succeed: the boundary head survived, so the trust boundary still resolves. \
        output: {}",
        out
    );
    assert!(!out.contains("tampered"), "no tampering accusation is expected: {}", out);

    // The pre-trust parcel reachable from `main` (`legacy` itself) must be counted as legacy, not
    // silently dropped from the tally — a `verified` audit that stopped mentioning legacy parcels
    // at all would still pass the two assertions above while losing the claim under test.
    assert!(
        out.contains("1 legacy parcel(s) predate trust and are unsigned"),
        "expected the legacy tally to count exactly the one pre-trust parcel reachable from \
        `main`: {}",
        out
    );
}

/// THE FIX (FORK-81): does the boundary head survive gc even when the *pallet ref* that
/// happened to reach it is dropped entirely — not merely undone — and does `audit` still pass?
///
/// The boundary is a **snapshot at enroll time**, so it can name a head no ref currently
/// reaches even without `undo`: create a second ref at an *ancestor* after the snapshot, then
/// drop the ref that held the boundary head outright (`delete_pallet_ref`, not a shipped verb —
/// this leg is the direct-deletion sibling of the CLI-only leg above). Before the fix, `b`'s
/// only tie to gc's live set was that ref, so dropping it collected `b` even though a signed
/// revocation-free anchor still named it as the boundary. Now `anchor.boundary` is a GC root in
/// its own right (`office_utils::collect_trust_pin_roots`), independent of any pallet ref, so
/// `b` survives regardless.
///
/// This was `a_legacy_parcel_outlives_the_boundary_head_that_attested_it`, and it was green
/// pinning the DEFECT (`b` collected, `legacy` outliving it, `audit` refusing with a tampering
/// accusation naming `legacy`). Reverting the production fix reddens `b_present` first.
#[test]
fn a_boundary_head_and_the_legacy_parcel_it_attested_both_survive_gc() {
    let warehouse = Warehouse::new_unenrolled("anchor");

    // Pre-trust, unsigned history on `main`: legacy <- b.
    let legacy = warehouse.stack("app.txt", "v1\n", "legacy one");
    let b = warehouse.stack("app.txt", "v2\n", "legacy two");

    // Enroll NOW. Only `main` exists, so the anchor's boundary is exactly [b].
    warehouse.run_ok(&["office", "enroll"]);

    let boundary = warehouse.scoped(|| {
        office_utils::read_trust_anchor().unwrap().expect("an anchor").boundary
    });

    assert_eq!(
        boundary,
        vec![b.clone()],
        "the fixture requires the boundary to be exactly [b], not to contain `legacy`"
    );

    // A second ref at `legacy`, created AFTER the snapshot: `legacy` is ref-reachable but is not
    // itself a boundary entry — its only attestation is being an ancestor of `b`.
    warehouse.run_ok(&["palletize", "keep", &legacy]);

    // Drop `main` outright. No ref reaches `b` any more; `legacy` stays alive through `keep`.
    warehouse.delete_pallet_ref("main");

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));

    let (b_present, legacy_present) = warehouse.scoped(|| {
        (
            file_utils::does_object_exist(&b).unwrap(),
            file_utils::does_object_exist(&legacy).unwrap(),
        )
    });

    let audit = warehouse.run(&["audit"]);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );

    println!(
        "FIXED: gc deleted {}; b present = {}; legacy present = {}; audit exit = {:?}\n{}",
        stats.deleted, b_present, legacy_present, audit.status.code(), out.trim()
    );

    assert!(b_present, "the boundary head must survive gc even with no ref reaching it");
    assert!(legacy_present, "the legacy parcel must survive through `keep`");

    // THE CLAIM UNDER TEST.
    assert!(
        audit.status.success(),
        "audit must succeed: the boundary head survived, so the trust boundary still resolves \
        and the legacy parcel is not falsely accused. output: {}",
        out
    );
    assert!(!out.contains("tampered"), "no tampering accusation is expected: {}", out);
    assert!(
        out.contains("1 legacy parcel(s) predate trust and are unsigned"),
        "expected the legacy tally to count exactly the one pre-trust parcel reachable from \
        `keep`: {}",
        out
    );
}

/// FORK-81 (finding 2): once a rooted pin can no longer be gc'd, what does an ACTUAL INTERIOR
/// gap in it mean, and what does `audit` say about it? A hash the boundary walk reaches as a
/// *parent* edge from a parcel it found present — this leg's `mid`, behind the present boundary
/// head `b` — excuses a parcel from the tampering accusation exactly the same way an absent
/// boundary HEAD does (see `audit_utils::verify_pallet_history`'s `boundary_gap` doc: any gap
/// the walk meets makes its negative undecidable, head or interior alike — the sibling leg
/// `a_genuinely_pre_trust_side_pallet_parcel_behind_an_absent_boundary_head_refuses_instead_of_
/// accusing` pins the head-only case). This leg constructs the interior case directly: `b`
/// stays present; `mid`, `b`'s own parent, is deleted instead — exactly as it would be from
/// genuine object loss, or a store that never fetched that far back.
///
/// The pallet actually audited must NOT itself need to read the deleted object's body — if it
/// did, `audit` would fail at the ordinary "this parcel's own body is missing" presence check
/// (`classify_parcel_trust`'s `object_utils::load_parcel`, which every discovered parcel of the
/// audited pallet must pass) before ever reaching the boundary-gap logic under test, which
/// would make this leg pass for the wrong reason. So `mid` and `b` belong to `main`, and the
/// pallet actually audited (`audit-target`) is palletized directly at `legacy`, `mid`'s
/// ancestor — neither `mid` nor `b` is ever part of `audit-target`'s own history, only of
/// `anchor.boundary`'s ancestry.
#[test]
fn an_absent_interior_ancestor_behind_a_present_boundary_head_is_named_not_accused_of_tampering() {
    let warehouse = Warehouse::new_unenrolled("interior-gap");

    // Pre-trust, unsigned history on `main`: legacy <- mid <- b.
    let legacy = warehouse.stack("app.txt", "v1\n", "legacy one");
    let mid = warehouse.stack("app.txt", "v2\n", "legacy two");
    let b = warehouse.stack("app.txt", "v3\n", "legacy three");

    // Enroll now. Only `main` exists, so the anchor's boundary is exactly [b].
    warehouse.run_ok(&["office", "enroll"]);

    let boundary = warehouse.scoped(|| {
        office_utils::read_trust_anchor().unwrap().expect("an anchor").boundary
    });
    assert_eq!(boundary, vec![b.clone()], "the fixture requires the boundary to be exactly [b]");

    // A second pallet at `legacy` — two hops behind `b`, via `mid` — so the pallet this leg
    // actually audits never needs `mid`'s or `b`'s own body to be present.
    warehouse.run_ok(&["palletize", "audit-target", &legacy]);

    // The interior gap: `b` (the boundary head) stays present; `mid`, reached only as a parent
    // edge from `b`, is deleted.
    warehouse.delete_object(&mid);

    let audit = warehouse.run(&["audit", "audit-target"]);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );

    println!("INTERIOR GAP: audit exit = {:?}\n{}", audit.status.code(), out.trim());

    assert!(
        !audit.status.success(),
        "audit must refuse: `legacy` cannot be proven pre-trust with `mid` absent. output: {}",
        out
    );
    assert!(
        !out.contains("tampered"),
        "no tampering accusation is expected — this store cannot resolve the boundary, it \
        never proved tampering. output: {}",
        out
    );
    assert!(
        !out.contains("was stacked after trust"),
        "the refusal must not claim `legacy` was stacked after trust — this store simply \
        cannot tell which side of trust it falls on. output: {}",
        out
    );
    assert!(
        out.contains(&mid),
        "the refusal must name the missing interior parcel {}: {}",
        mid, out
    );
    assert!(
        out.contains(&legacy),
        "the refusal must name the parcel actually under audit {}: {}",
        legacy, out
    );
}

/// FORK-81 (finding 2), reverted narrowing (PR #121 round 2) — a fixture whose parcel's status
/// is genuinely ambiguous to the walk, replacing `a_genuine_violation_beside_an_unrelated_
/// absent_head_is_accused_of_tampering` (PR #121 round 1), which was NOT discriminating: that
/// fixture stripped `outside`'s signature directly, so the test itself knew a violation the
/// walk could never actually see — asserting "the accusation still fires" proved nothing about
/// whether the absent head was truly irrelevant, only that a hand-forced violation stayed a
/// violation. This leg instead constructs a parcel — `s0` — that is genuinely, unfalsifiably
/// pre-trust, and makes exactly the boundary entry that could prove it absent.
///
/// `side` stacks TWO unsigned parcels off `legacy` (`s0` then `s`, `side`'s new head) before
/// enroll. `office enroll` snapshots every local pallet head, so `anchor.boundary = [b, s]` —
/// `s`, not `s0`. A third pallet, `probe`, is palletized at `s0` itself (an ancestor of `s`, not
/// `s` itself), and then `s` — one of the boundary's own two entries — is deleted directly (as
/// if never fetched, or genuinely lost).
///
/// `s0` sits ONE hop behind the deleted `s`: `s0` is `s`'s own parent, so if `s` were present
/// the walk would trivially reach `s0` and settle it as legacy. But `s` is absent, and the walk
/// can never even look past it — `b`, the OTHER boundary entry, is on a completely different
/// branch (off `legacy` toward `main`, not `side`) and never reaches `s0` either. So this store
/// genuinely cannot tell whether `s0` predates trust (it does) or was stacked after it (it
/// wasn't) — the honest "cannot resolve" refusal is the only answer that does not overclaim,
/// and it is what any-gap semantics gives. PR #121 round 1's interior-only narrowing would
/// instead ACCUSE `s0` of tampering here: `s`, the ONE entry that could have vouched for it, is
/// a head-only gap, so `boundary_gap` under that narrowing is never set, and a genuinely
/// innocent, never-tampered parcel gets branded — the exact false positive FORK-81 exists to
/// prevent.
///
/// Resolution: `s`'s bytes (captured before deletion) are written straight back — standing in
/// for fetching it from wherever it actually lives, the way `lift`ing a pallet would in a real
/// remote scenario (see the two server-backed constructions in `tests/remote.rs`, which use a
/// real remote for exactly this reason; this leg is local-only and has no remote to lift from,
/// so restoring the object directly is the faithful analogue). The identical audit then
/// succeeds and reports `s0` as legacy.
#[test]
fn a_genuinely_pre_trust_side_pallet_parcel_behind_an_absent_boundary_head_refuses_instead_of_accusing() {
    let warehouse = Warehouse::new_unenrolled("probe-s0");

    // Pre-trust, unsigned history: legacy <- b, on `main`.
    let legacy = warehouse.stack("app.txt", "v1\n", "legacy one");
    let b = warehouse.stack("app.txt", "v2\n", "legacy two");

    // A second, unrelated pre-trust pallet: side, at legacy <- s0 <- s (two hops).
    warehouse.run_ok(&["palletize", "side", &legacy]);
    let s0 = warehouse.stack("side.txt", "s0\n", "side work 1");
    let s = warehouse.stack("side.txt", "s1\n", "side work 2");
    warehouse.run_ok(&["shift", "main"]);

    // Enroll now: both pallets exist, so the anchor's boundary is [b, s] — side's CURRENT
    // head, not s0.
    warehouse.run_ok(&["office", "enroll"]);

    let boundary = warehouse.scoped(|| {
        office_utils::read_trust_anchor().unwrap().expect("an anchor").boundary
    });
    assert_eq!(
        boundary, vec![b.clone(), s.clone()],
        "the fixture requires the boundary to be exactly [b, s]"
    );

    // A third pallet, "probe", at s0 — an ancestor of side's boundary snapshot s, not s itself
    // — so the pallet actually audited never needs s's own body to be present.
    warehouse.run_ok(&["palletize", "probe", &s0]);

    // The absent boundary head: s (one of anchor.boundary's own two entries) is deleted
    // directly, as if never fetched or genuinely lost. Its bytes are kept so resolution below
    // can restore them without re-deriving the exact same content-addressed object.
    let s_path = warehouse.root.join(".forklift").join("objects").join(&s[..2]).join(&s[2..]);
    let s_bytes = std::fs::read(&s_path).expect("s's object exists before deletion");
    warehouse.delete_object(&s);

    let audit = warehouse.run(&["audit", "probe"]);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );

    println!("PROBE @ S0: audit exit = {:?}\n{}", audit.status.code(), out.trim());

    assert!(
        !audit.status.success(),
        "audit must refuse: `s0` cannot be proven pre-trust with `s` absent. output: {}",
        out
    );
    assert!(
        !out.to_lowercase().contains("tampered"),
        "no tampering accusation is expected — this store cannot resolve the boundary, it \
        never proved tampering. output: {}",
        out
    );
    assert!(
        out.contains(&s),
        "the refusal must name the missing boundary parcel {}: {}",
        s, out
    );
    assert!(
        out.contains(&s0),
        "the refusal must name the parcel actually under audit {}: {}",
        s0, out
    );

    // Resolution: supply the missing head (the local analogue of lifting/fetching it) and
    // re-run the identical audit.
    std::fs::write(&s_path, &s_bytes).expect("restoring s's object");

    let resolved = warehouse.run(&["audit", "probe"]);
    let resolved_out = format!(
        "{}{}",
        String::from_utf8_lossy(&resolved.stdout),
        String::from_utf8_lossy(&resolved.stderr)
    );

    assert!(
        resolved.status.success(),
        "with s restored, the identical audit must now succeed: {}",
        resolved_out
    );
    // `probe`'s own history is `legacy <- s0` (side was palletized from `legacy`), so the
    // resolved audit reports two legacy parcels, not one — `legacy` was always present and
    // never in question; `s0` is the one this leg is actually about.
    assert!(
        resolved_out.contains("2 legacy parcel(s) predate trust and are unsigned"),
        "s0 (and legacy, its own ancestor) must now report as legacy: {}",
        resolved_out
    );
}

/// The positive control: nothing else in this suite pins the accusation actually firing.
/// `"was stacked after trust"` appears only as a NEGATIVE assertion across every other
/// trust-boundary leg here — each of them is specifically about a refusal, since any-gap
/// semantics (PR #121 round 2) means EVERY gap of either kind makes the walk's negative
/// undecidable, never merely "irrelevant" — so a regression that silenced
/// `verify_pallet_history`'s accusation arm outright would leave this suite green. This leg has
/// no gap of either kind: every boundary entry stays present, and the accusation still fires,
/// naming the violating parcel.
#[test]
fn the_accusation_still_fires_with_no_gap_at_all() {
    let warehouse = Warehouse::new_unenrolled("no-gap");

    // Pre-trust, unsigned history: legacy <- b, on `main`. `legacy` itself is not asserted on
    // below (unlike the other legs) — this leg only cares that the boundary resolves cleanly
    // and that the genuine violation is still accused.
    warehouse.stack("app.txt", "v1\n", "legacy one");
    let b = warehouse.stack("app.txt", "v2\n", "legacy two");

    // Enroll now. Only `main` exists, so the anchor's boundary is exactly [b].
    warehouse.run_ok(&["office", "enroll"]);

    let boundary = warehouse.scoped(|| {
        office_utils::read_trust_anchor().unwrap().expect("an anchor").boundary
    });
    assert_eq!(boundary, vec![b.clone()], "the fixture requires the boundary to be exactly [b]");

    // The genuine violation: signed at `stack` time (trust is established), then its signature
    // is stripped directly. Nothing else in the store is touched — the boundary resolves
    // cleanly, with no gap of either kind.
    let outside = warehouse.stack("app.txt", "v3\n", "after trust");
    warehouse.delete_signature(&outside);

    let audit = warehouse.run(&["audit", "main"]);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&audit.stdout),
        String::from_utf8_lossy(&audit.stderr)
    );

    println!("NO GAP: audit exit = {:?}\n{}", audit.status.code(), out.trim());

    assert!(
        !audit.status.success(),
        "audit must refuse: `outside` is a genuine violation. output: {}",
        out
    );
    assert!(
        out.contains("tampered"),
        "a clean boundary walk (no gap at all) must still accuse a genuine violation: {}",
        out
    );
    assert!(
        out.contains(&outside),
        "the accusation must name the actual violation {}: {}",
        outside, out
    );
}

/// Builds FORK-82's fixture up to a conflicted pick: `source` sits on no pallet (an `undo` moved
/// `feature` back off it, but the object is still on disk), `main` touched the same file, and the
/// pick is left in progress with its state on disk. Returns `(canary, source)`; the canary is
/// pinned by nothing, so its collection witnesses that a sweep ran.
fn warehouse_with_a_pick_of_an_orphaned_source(warehouse: &Warehouse) -> (String, String) {
    warehouse.stack("app.txt", "base\n", "base");

    warehouse.run_ok(&["palletize", "canary"]);
    let canary = warehouse.stack("canary.txt", "c1\n", "canary work");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.delete_pallet_ref("canary");

    // No shift between the stack and the undo: `shift` is journaled, and one here would make
    // `undo` revert the shift instead. `undo` is soft, so the feature content is discarded before
    // the shift back.
    warehouse.run_ok(&["palletize", "feature"]);
    let source = warehouse.stack("app.txt", "from-feature\n", "feature work");
    warehouse.run_ok(&["undo"]);
    warehouse.run_ok(&["restore", "--staged", "."]);
    warehouse.run_ok(&["restore", "."]);
    warehouse.run_ok(&["shift", "main"]);

    warehouse.stack("app.txt", "from-main\n", "main work");

    let pick = warehouse.run(&["cherry-pick", &source]);
    assert!(
        pick_state_file(warehouse).exists(),
        "the pick must be left in progress with its state on disk; it did not conflict, so the \
         fixture is wrong. cherry-pick stderr: {}",
        String::from_utf8_lossy(&pick.stderr)
    );

    (canary, source)
}

fn pick_state_file(warehouse: &Warehouse) -> PathBuf {
    warehouse.root.join(".forklift").join("cherry-pick")
}

/// FORK-82 — THE ACTING READER, fixed by rooting. `CherryPickState.source` is a durable parcel
/// hash the completing `stack` acts on (`collect_source_authors` loads it), and no ref need reach
/// it. Unrooted, a collection between the conflicted pick and its completion deleted it, and the
/// completing `stack` died on a raw object-read error naming neither the pick nor a way out. A pick
/// in progress now roots its source (`bay_utils::read_one_bay_scoped_roots`), so it survives a
/// collection that sweeps the canary beside it, and the pick completes.
#[test]
fn a_cherry_pick_in_progress_roots_its_source_and_the_completing_stack_succeeds() {
    let warehouse = Warehouse::new("pick");
    let (canary, source) = warehouse_with_a_pick_of_an_orphaned_source(&warehouse);

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(
        warehouse.scoped(|| file_utils::does_object_exist(&source).unwrap()),
        "the source of the pick in progress was collected: a pick must root its source"
    );

    std::fs::write(warehouse.root.join("app.txt"), "resolved\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    let completing = warehouse.run(&["stack", "completed pick"]);

    assert!(
        completing.status.success(),
        "the completing stack failed over a rooted source; stderr: {}",
        String::from_utf8_lossy(&completing.stderr)
    );
    assert!(!pick_state_file(&warehouse).exists(), "the completing stack must clear the pick state");
}

/// FORK-82's residue: rooting covers a pick while it is in progress, but `undo` of the stack that
/// completed a pick would put the pick back from the journal, which is no root. The completing
/// parcel is single-parent, so between the completion and the undo nothing names the source, and a
/// collection there deletes it. A pick put back without its source could never complete, so the
/// undo is refused before it changes anything, and the way out it names (stack on top) works.
#[test]
fn undoing_a_completed_pick_whose_source_was_collected_is_refused_and_changes_nothing() {
    let warehouse = Warehouse::new("pick-undone");
    let (canary, source) = warehouse_with_a_pick_of_an_orphaned_source(&warehouse);

    std::fs::write(warehouse.root.join("app.txt"), "resolved\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "completed pick"]);
    assert!(!pick_state_file(&warehouse).exists(), "the completing stack must clear the pick state");

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(
        !warehouse.scoped(|| file_utils::does_object_exist(&source).unwrap()),
        "the source survived a collection while no pick named it, so this leg tests nothing"
    );

    let head = warehouse.head("main");
    let undo = warehouse.run(&["undo"]);
    let err = String::from_utf8_lossy(&undo.stderr).to_string();

    assert!(!pick_state_file(&warehouse).exists(), "the refused undo put the pick back; stderr: {}", err);
    assert_eq!(warehouse.head("main"), head, "the refused undo moved the pallet; stderr: {}", err);
    assert!(!undo.status.success(), "undo put back a pick whose source is gone");
    assert!(
        err.contains(&format!("would put the cherry-pick of {} back in progress", source))
            && err.contains("Nothing was undone"),
        "the refusal must name the pick and say nothing changed; stderr: {}",
        err
    );

    // The refusal consumed no journal entry: a second undo meets the same entry, not an older one.
    let again = warehouse.run(&["undo"]);
    assert!(
        !again.status.success() && String::from_utf8_lossy(&again.stderr).contains(&format!("cherry-pick of {}", source)),
        "a second undo must meet the same refused entry; stderr: {}",
        String::from_utf8_lossy(&again.stderr)
    );

    std::fs::write(warehouse.root.join("app.txt"), "on top\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "on top"]);
}

/// The stack-time half: a pick in progress whose source the store lost anyway (damage; gc can no
/// longer produce it) refuses naming the pick, its source and the abort, instead of the raw
/// object-read error that named none of them, and the abort it names works.
#[test]
fn a_pick_whose_source_was_lost_refuses_naming_the_pick_and_the_abort_works() {
    let warehouse = Warehouse::new("pick-lost");
    let (_, source) = warehouse_with_a_pick_of_an_orphaned_source(&warehouse);
    warehouse.delete_object(&source);

    std::fs::write(warehouse.root.join("app.txt"), "resolved\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    let head = warehouse.head("main");

    let completing = warehouse.run(&["stack", "completed pick"]);
    let err = String::from_utf8_lossy(&completing.stderr).to_string();

    assert_eq!(warehouse.head("main"), head, "a refused stack must leave the pallet where it was");
    assert!(!completing.status.success(), "the stack completed a pick whose source is gone");
    assert!(
        err.contains("The cherry-pick in progress cannot be completed") && err.contains(&source),
        "the refusal must name the pick and its source; stderr: {}",
        err
    );
    assert!(
        err.contains(&format!("Abort the pick (remove \"{}\")", PathBuf::from(".forklift").join("cherry-pick").display())),
        "the refusal must name the abort; stderr: {}",
        err
    );

    std::fs::remove_file(pick_state_file(&warehouse)).unwrap();
    warehouse.run_ok(&["stack", "completed pick"]);

    // As the refusal says, the stacked changes record the operator as their only author. With the
    // source gone its authors cannot be preserved, so the claim's whole content is one Author,
    // the stacker.
    let parcel = warehouse.scoped(|| object_utils::load_parcel(&warehouse.head("main")).unwrap());
    let authors: Vec<&str> = parcel.actions.iter()
        .filter(|action| matches!(action.action, ParcelActionType::Author))
        .map(|action| action.operator.identifier.as_str())
        .collect();
    let stackers: Vec<&str> = parcel.actions.iter()
        .filter(|action| matches!(action.action, ParcelActionType::Stack))
        .map(|action| action.operator.identifier.as_str())
        .collect();
    assert_eq!(stackers.len(), 1, "one stacker: {:?}", stackers);
    assert_eq!(authors, stackers, "the stacker must be the only author after the abort");
}

/// FORK-83 — THE HEALING READER, fixed: an absent tag subject no longer wedges the warehouse.
///
/// A tag is not a gc root, so `undo` plus a collection can sweep a tagged parcel; a sparse
/// `franchise --only` also adopts every tag record without the history behind it. Locally the two
/// cannot be told apart, and neither can ever be resolved. Before the fix, heal's walk rooted tag
/// subjects at the ordinary reporting policy, so a torn taint's store-wide rescan named the
/// absent subject as dangling and refused, with exit 21, on every command. There was no exit
/// either: `tag` has no retire verb, and the refusal pointed back at `forklift heal`. Heal no
/// longer roots tags at all, exactly like gc (`recovery_utils::WalkRoots::tag_subjects`): it
/// clears, and names the tag in a note.
///
/// The taint is written directly rather than driven through a command because a torn taint is by
/// definition crash debris: `parse_taint_content` calls a file torn when it lacks the `END\n`
/// suffix, which is exactly what a crash mid-write leaves. Everything else here is a shipped
/// command.
///
/// Mutation: queue tag subjects at `AbsencePolicy::Report` in `walk_closure_for` → the rescan
/// names the subject dangling → `heal` exits 21 → red at the first assertion. Dropping the
/// tag-note loop in `heal_torn_taint` instead → red at the `Note:` assertion.
#[test]
fn a_torn_taint_over_an_absent_tag_subject_heals_and_names_the_tag() {
    let warehouse = Warehouse::new("torn");

    warehouse.stack("app.txt", "v1\n", "first");

    // The canary, as in every other leg.
    warehouse.run_ok(&["palletize", "canary"]);
    let canary = warehouse.stack("canary.txt", "c1\n", "canary work");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.delete_pallet_ref("canary");

    let tagged = warehouse.stack("app.txt", "v2\n", "second");
    warehouse.run_ok(&["tag", "create", "v1.0", &tagged, "-m", "release"]);

    // Orphan and collect the subject: `undo` moves the head back past the tagged parcel.
    warehouse.run_ok(&["undo"]);

    // A soft `undo` leaves the undone content staged. Unstaging keeps the tag subject the only
    // absent root this leg is about.
    warehouse.run_ok(&["restore", "--staged", "."]);

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    let present = warehouse.scoped(|| file_utils::does_object_exist(&tagged).unwrap());

    println!(
        "TORN: gc deleted {} object(s); tag subject {} present = {}\n      canary was {}",
        stats.deleted, tagged, present, canary
    );
    // FIXTURE PRECONDITIONS, not claims: the store really reached the state under test. Rooting
    // tag subjects in gc would redden here instead — a remedy FORK-83 rules out.
    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(!present, "the fixture needs the tag subject collected");

    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 1\nobjects/ab/cdef\n").unwrap();

    let healed = warehouse.run(&["heal"]);
    let heal_out = String::from_utf8_lossy(&healed.stdout).to_string();
    let heal_err = String::from_utf8_lossy(&healed.stderr).to_string();
    println!(
        "TORN: `heal` exit = {:?}\n  stdout: {}\n  stderr: {}",
        healed.status.code(), heal_out.trim(), heal_err.trim()
    );

    assert!(
        healed.status.success(),
        "a torn taint whose only absent root is a tag subject must heal; got {:?}, stderr: {}",
        healed.status.code(), heal_err
    );
    let note = heal_out.lines().find(|line| line.starts_with("Note: tag \"v1.0\""));
    assert!(
        note.is_some_and(|line| line.contains(&tagged)),
        "heal must name the tag and its absent subject {} in a note. stdout: {}",
        tagged, heal_out
    );

    let ordinary = warehouse.run(&["stocktake"]);
    assert!(
        ordinary.status.success(),
        "with the taint cleared, ordinary commands must work again; stderr: {}",
        String::from_utf8_lossy(&ordinary.stderr)
    );
    // The tag record itself is untouched: still listed, its subject still absent.
    let listed = warehouse.run_ok(&["tag", "list"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("v1.0"),
        "heal must not touch the tag record. stdout: {}",
        String::from_utf8_lossy(&listed.stdout)
    );

    // `tag` still has no retire verb. That is no longer a wedge — the store heals around the
    // absent subject — but it is what made the old wedge permanent, so the set stays pinned.
    // Matched against the SUBCOMMAND LIST only, not the whole help text.
    let tag_help = warehouse.run(&["tag", "--help"]);
    let help = String::from_utf8_lossy(&tag_help.stdout).to_string();
    let subcommands: Vec<String> = help
        .lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        // Exactly two leading spaces then a non-space: clap indents a wrapped description's
        // continuation line far deeper, and the looser test would read it as a subcommand.
        .take_while(|line| line.starts_with("  ") && !line.starts_with("   "))
        .map(|line| line.trim().split_whitespace().next().unwrap_or("").to_string())
        .collect();
    println!("TORN: `tag` subcommands = {:?}", subcommands);
    assert_eq!(
        subcommands, vec!["create", "show", "list"],
        "`tag`'s subcommand list changed. Update the expected set, and if a retire verb landed, \
         say in FORK-83's ledger row that a tag can now be retired."
    );
}

/// The targeted path: a complete (not torn) taint whose recorded path IS the collected tag
/// subject. Heal checks that one hash against every root; only the tag names it, and a tag is not
/// a gc root, so holding the taint open over it would wedge for nothing.
///
/// Mutation: queue tag subjects as roots in `walk_closure_for` → the subject counts as
/// referenced → `heal` exits 21 → red. Dropping the tag-note loop on this targeted path
/// (`resolve_the_rest`) instead → red at the `Note:` assertion.
#[test]
fn a_recorded_taint_over_a_collected_tag_subject_clears() {
    let warehouse = Warehouse::new("tag-target");

    warehouse.stack("app.txt", "v1\n", "first");
    let tagged = warehouse.stack("app.txt", "v2\n", "second");
    warehouse.run_ok(&["tag", "create", "v1.0", &tagged, "-m", "release"]);
    warehouse.run_ok(&["undo"]);
    warehouse.run_ok(&["restore", "--staged", "."]);
    warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    assert!(
        !warehouse.scoped(|| file_utils::does_object_exist(&tagged).unwrap()),
        "the fixture needs the tag subject collected"
    );

    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    let record = forklift_core::util::taint_utils::taint_file_bytes([format!("objects/{}/{}", &tagged[..2], &tagged[2..])]);
    std::fs::write(taint_dir.join("taint-99999-0"), record).unwrap();

    let healed = warehouse.run(&["heal"]);
    println!(
        "TAG-TARGET: `heal` exit = {:?}\n  stdout: {}\n  stderr: {}",
        healed.status.code(),
        String::from_utf8_lossy(&healed.stdout).trim(),
        String::from_utf8_lossy(&healed.stderr).trim()
    );
    assert!(
        healed.status.success(),
        "a recorded taint over a hash only a tag names must clear; stderr: {}",
        String::from_utf8_lossy(&healed.stderr)
    );
    let heal_out = String::from_utf8_lossy(&healed.stdout).to_string();
    assert!(
        heal_out.lines().any(|line| line.starts_with("Note: tag \"v1.0\"")),
        "the targeted path must name the absent tag subject too; stdout: {}", heal_out
    );
    warehouse.run_ok(&["stocktake"]);
}

/// A tag history that cannot be read at all never wedges either. A `@tags` record syncs in
/// unvalidated, so a foreign client can publish one `parse_tag` rejects; tags feed heal's notes
/// only, so heal notes the failed read and clears rather than aborting its walk. The malformed
/// record is built directly because no shipped command writes one.
///
/// Mutation: propagate the `read_tags` error in `collect_walk_roots` → the rescan aborts →
/// `heal` exits 21 → red at the exit-code assertion. Dropping the `tags_unread` note on the
/// torn path, or on the targeted path (`resolve_the_rest`) → red at that path's `Note:` assertion.
#[test]
fn a_torn_taint_heals_over_a_malformed_tag_record_and_notes_it() {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::model::blob::Blob;
    use forklift_core::model::parcel::Parcel;
    use forklift_core::model::tree_item::TreeItem;
    use forklift_core::util::{pallet_utils, tag_utils};

    let warehouse = Warehouse::new("tag-malformed");

    let head = warehouse.stack("app.txt", "v1\n", "first");
    warehouse.run_ok(&["tag", "create", "v1.0", &head, "-m", "release"]);

    warehouse.scoped(|| {
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: b"name = \"bad\"\n".to_vec() });
        blob.store().unwrap();
        let mut node = TreeItem::new("bad.toml".to_string(), blob.hash.clone(), DirEntryType::Normal);
        for name in ["tags", "tracked", ".forklift", ""] {
            let mut tree = TreeItem::new(name.to_string(), String::new(), DirEntryType::Tree);
            tree.add_child(node);
            let mut object = LooseObjectBuilder::build_tree(&tree);
            object.store().unwrap();
            tree.hash = object.hash.clone();
            node = tree;
        }
        let parent = pallet_utils::get_meta_pallet_head(tag_utils::TAGS_PALLET_NAME).unwrap().unwrap();
        let mut parcel = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: node.hash.clone(), parents: vec![parent], actions: Vec::new(), description: None,
        });
        parcel.store().unwrap();
        pallet_utils::update_meta_pallet_head(
            tag_utils::TAGS_PALLET_NAME,
            pallet_utils::get_meta_pallet_head(tag_utils::TAGS_PALLET_NAME).unwrap().as_deref(),
            &parcel.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
        assert!(tag_utils::read_tags().is_err(), "the fixture needs an unreadable tag history");
    });

    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 1\nobjects/ab/cdef\n").unwrap();

    let healed = warehouse.run(&["heal"]);
    let heal_out = String::from_utf8_lossy(&healed.stdout).to_string();
    let heal_err = String::from_utf8_lossy(&healed.stderr).to_string();
    println!(
        "TAG-MALFORMED: `heal` exit = {:?}\n  stdout: {}\n  stderr: {}",
        healed.status.code(), heal_out.trim(), heal_err.trim()
    );
    assert!(
        healed.status.success(),
        "an unreadable tag history must not abort heal; got {:?}, stderr: {}",
        healed.status.code(), heal_err
    );
    assert!(
        heal_out.lines().any(|line| line.starts_with("Note: the tags could not be read")),
        "heal must say the tags went unchecked; stdout: {}", heal_out
    );

    // The targeted path too: a complete record naming an object nothing references.
    let record = forklift_core::util::taint_utils::taint_file_bytes([format!("objects/{}/{}", "a".repeat(2), "a".repeat(62))]);
    std::fs::write(taint_dir.join("taint-99999-1"), record).unwrap();
    let healed = warehouse.run(&["heal"]);
    let heal_out = String::from_utf8_lossy(&healed.stdout).to_string();
    assert!(
        healed.status.success(),
        "the targeted path must not abort on an unreadable tag history either; stderr: {}",
        String::from_utf8_lossy(&healed.stderr)
    );
    assert!(
        heal_out.lines().any(|line| line.starts_with("Note: the tags could not be read")),
        "the targeted path must say the tags went unchecked too; stdout: {}", heal_out
    );
}

/// The other direction: the tag's policy must not swallow a genuine loss.
///
/// The tagged parcel is the parent of `main`'s head, so it is reached by an ancestry edge as
/// well as by the tag, and its absence is real local loss (gc cannot produce it; it is deleted
/// directly, as in the FORK-81 legs, so no canary is used). The rescan must still report it and
/// refuse.
///
/// Mutation: the over-broad fix — have `enumerate_absent_reachable`'s sink drop any hash a tag
/// names → nothing is reported → `heal` clears → red at the exit-code assertion.
#[test]
fn a_lost_tagged_parcel_that_is_also_an_ancestor_still_refuses() {
    let warehouse = Warehouse::new("torn-tag-ancestor");

    let tagged = warehouse.stack("app.txt", "v1\n", "first");
    warehouse.run_ok(&["tag", "create", "v1.0", &tagged, "-m", "release"]);
    warehouse.stack("app.txt", "v2\n", "second");

    warehouse.delete_object(&tagged);

    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 1\nobjects/ab/cdef\n").unwrap();

    let healed = warehouse.run(&["heal"]);
    let heal_err = String::from_utf8_lossy(&healed.stderr).to_string();
    println!(
        "TORN-ANCESTOR: `heal` exit = {:?}\n  stdout: {}\n  stderr: {}",
        healed.status.code(),
        String::from_utf8_lossy(&healed.stdout).trim(),
        heal_err.trim()
    );

    assert_eq!(
        healed.status.code(), Some(21),
        "a lost parcel that `main`'s history still reaches must refuse, tag or no tag; got {:?}, \
         stderr: {}",
        healed.status.code(), heal_err
    );
    assert!(
        heal_err.contains(&tagged),
        "the refusal must name the lost parcel {}. stderr: {}",
        tagged, heal_err
    );
}

/// CONTROL: a torn taint with nothing dangling heals cleanly.
///
/// A narrow control: no tag, no `undo`, no collection. Its green shows only that torn-ness
/// *alone* does not refuse, so `a_lost_tagged_parcel_that_is_also_an_ancestor_still_refuses`
/// cannot be satisfied by "a torn taint refuses any warehouse". It says nothing about tags.
///
/// Before FORK-156 a collected staged blob wedged the store with no tag at all; chasing an
/// unexplained second dangling hash in the old FORK-83 leg is what found that defect, now pinned
/// fixed by `staged_content_survives_collection_and_a_torn_taint_heals_over_it`.
#[test]
fn a_torn_taint_alone_on_an_untouched_warehouse_is_the_control() {
    let warehouse = Warehouse::new("torn-control");

    warehouse.stack("app.txt", "v1\n", "first");

    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 1\nobjects/ab/cdef\n").unwrap();

    let healed = warehouse.run(&["heal"]);
    println!(
        "CONTROL: `heal` exit = {:?}\n  stdout: {}\n  stderr: {}",
        healed.status.code(),
        String::from_utf8_lossy(&healed.stdout).trim(),
        String::from_utf8_lossy(&healed.stderr).trim()
    );

    let ordinary = warehouse.run(&["stocktake"]);
    println!(
        "CONTROL: `stocktake` exit = {:?}\n  stderr: {}",
        ordinary.status.code(),
        String::from_utf8_lossy(&ordinary.stderr).trim()
    );

    // Same torn taint, same shipped commands, nothing absent: the store heals and stays usable.
    assert!(
        healed.status.success(),
        "a torn taint alone must heal cleanly — if this refuses, every refusal in the FORK-83 legs \
         is explained by torn-ness, not by what is absent. stdout: {} / stderr: {}",
        String::from_utf8_lossy(&healed.stdout), String::from_utf8_lossy(&healed.stderr)
    );
    assert!(
        ordinary.status.success(),
        "with the taint cleared, ordinary commands must work again; stderr: {}",
        String::from_utf8_lossy(&ordinary.stderr)
    );
}

/// FORK-156 (found as FORK-83's widened half) — STAGED CONTENT IS A GC ROOT.
///
/// `load` a file and do not stack it yet: the blob is referenced by the bay's staged inventory
/// shard and by nothing else. gc's live set used to leave staged shards unrooted while heal's walk
/// rooted them, and the asymmetry was recorded as accepted. It was not harmless: collection
/// deleted the staged blob, and a later torn taint then wedged the whole warehouse, because the
/// rescan folds every referenced-but-absent hash into its remainder and entry-heal blocks
/// `restore --staged`, the one command that would have dropped the reference.
///
/// With staged shards rooted, the blob survives collection, so a torn taint over this store heals
/// like the untouched control and the staged work is still restorable. Names the blob `load`
/// created rather than counting deletions, so the leg is about that object and nothing else.
#[test]
fn staged_content_survives_collection_and_a_torn_taint_heals_over_it() {
    let warehouse = Warehouse::new("staged");

    warehouse.stack("app.txt", "v1\n", "first");

    let before = warehouse.loose_objects();
    std::fs::write(warehouse.root.join("wip.txt"), "work in progress\n").unwrap();
    warehouse.run_ok(&["load", "."]);

    let staged: Vec<String> =
        warehouse.loose_objects().difference(&before).cloned().collect();
    println!("STAGED: `load` created {:?}", staged);
    let wip_blob = match staged.as_slice() {
        [only] => only.clone(),
        other => panic!(
            "expected `load` to create exactly one object (the staged blob) so this leg can name \
             it; got {:?}", other
        ),
    };

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    println!("STAGED: gc deleted {} object(s)", stats.deleted);
    assert!(
        warehouse.scoped(|| file_utils::does_object_exist(&wip_blob).unwrap()),
        "gc must keep the staged-but-unstacked blob {}: a bay's staged inventory is a gc root \
         (gc deleted {} object(s))",
        wip_blob, stats.deleted
    );

    // The torn taint: crash debris, per `parse_taint_content`.
    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 1\nobjects/ab/cdef\n").unwrap();

    let healed = warehouse.run(&["heal"]);
    println!("STAGED: heal exit = {:?}\n  stderr: {}",
        healed.status.code(), String::from_utf8_lossy(&healed.stderr).trim());
    assert!(
        healed.status.success(),
        "with the staged blob kept, a torn taint must heal exactly like the untouched control. \
         stdout: {} / stderr: {}",
        String::from_utf8_lossy(&healed.stdout), String::from_utf8_lossy(&healed.stderr)
    );

    let stocktake = warehouse.run_ok(&["stocktake"]);
    assert!(
        String::from_utf8_lossy(&stocktake.stdout).contains("wip.txt"),
        "wip.txt must still be staged after the heal; stdout: {}",
        String::from_utf8_lossy(&stocktake.stdout)
    );

    std::fs::remove_file(warehouse.root.join("wip.txt")).unwrap();
    warehouse.run_ok(&["restore", "wip.txt"]);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("wip.txt")).unwrap(),
        "work in progress\n",
        "the staged work must be restorable from the store after collection and heal"
    );
}

/// FORK-156 — the silent-loss path, through shipped commands only. A full repack drops non-live
/// packed objects with no grace period, and automatic maintenance runs one when packs pile up.
/// Before staged shards were rooted, this sequence deleted the staged blob while `stocktake` still
/// listed the file as staged; `stack` then signed a parcel over the missing hash, `audit` passed
/// it, and only a later `restore` failed. Asserts on the content coming back rather than on the
/// test process's own view of the packs, which a repack in a CLI subprocess does not refresh.
#[test]
fn a_full_repack_keeps_staged_content_and_the_next_stack_restores_it() {
    let warehouse = Warehouse::new("staged-repack");

    warehouse.stack("app.txt", "v1\n", "first");

    std::fs::write(warehouse.root.join("wip.txt"), "work in progress\n").unwrap();
    warehouse.run_ok(&["load", "."]);

    // Pack the staged blob first, so the full repack below has to decide whether to carry it.
    warehouse.run_ok(&["compact"]);
    warehouse.run_ok(&["compact", "--all"]);

    warehouse.run_ok(&["stack", "second"]);
    warehouse.run_ok(&["audit"]);

    std::fs::remove_file(warehouse.root.join("wip.txt")).unwrap();
    let restored = warehouse.run(&["restore", "wip.txt"]);
    assert!(
        restored.status.success(),
        "a file staged before a full repack must still be restorable after it is stacked — if \
         this fails the repack dropped the staged blob. stderr: {}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("wip.txt")).unwrap(),
        "work in progress\n"
    );
}

/// FORK-213's T4: a bay's base is a gc root. Bay b catches up onto A's parcel N, A undoes that
/// stack (main back at X), and nothing else names N; b's catch-up back to X still needs N's tree
/// to diff against. Revert: drop the base from the bay-scoped roots → N is collected and the
/// catch-up fails.
#[test]
fn a_bays_base_survives_collection_after_the_pallet_moved_back() {
    let warehouse = Warehouse::new("bay-base");
    let x = warehouse.stack("f.txt", "f v1\n", "first");

    let bay = warehouse.home.join("bay-b");
    warehouse.run_ok(&["bay", "add", "b", bay.to_str().unwrap()]);
    let run_in_bay = |args: &[&str]| {
        forklift_command()
            .args(args)
            .current_dir(&bay)
            .env("FORKLIFT_GLOBAL_CONFIG", warehouse.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", warehouse.home.join("keys"))
            .output()
            .unwrap()
    };
    assert!(run_in_bay(&["shift", "main"]).status.success());

    let n = warehouse.stack("f.txt", "f v2\n", "second");
    let caught_up = run_in_bay(&["shift", "main"]);
    assert!(caught_up.status.success(), "{}", String::from_utf8_lossy(&caught_up.stderr));
    warehouse.run_ok(&["undo"]);
    assert_eq!(warehouse.head("main"), x);

    warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    assert!(warehouse.scoped(|| file_utils::does_object_exist(&n).unwrap()), "gc must keep bay b's base {}", n);

    let back = run_in_bay(&["shift", "main"]);
    assert!(back.status.success(), "{}", String::from_utf8_lossy(&back.stderr));
    assert_eq!(std::fs::read_to_string(bay.join("f.txt")).unwrap(), "f v1\n");
}

/// Load `file` with `content` and return every loose object the load created, sorted. Staged
/// objects are rooted, so these are exactly the objects the bay's inventory now names. The file's
/// mtime is moved an hour back first, out of the racily-clean window, so every later `load` (and
/// `park`'s refresh) proves it unchanged by its stat data alone and stores nothing.
fn load_and_name_staged(warehouse: &Warehouse, file: &str, content: &[u8]) -> Vec<String> {
    let before = warehouse.loose_objects();
    let path = warehouse.root.join(file);
    std::fs::write(&path, content).unwrap();
    std::fs::OpenOptions::new().write(true).open(&path).unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.loose_objects().difference(&before).cloned().collect()
}

/// Bytes past the chunking threshold that no two chunks share, so a chunked load stores a recipe
/// and several distinct chunks.
fn chunkable_content() -> Vec<u8> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..9 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }).collect()
}

/// FORK-157 — a staged object lost by a route gc no longer takes (crash debris, a manual
/// deletion) must stop `stack` and `park`, not end up named by a signed parcel. The staged item was
/// loaded *here*, so its absence is loss, not a sparse gap; nothing after the build can tell the
/// two apart (`audit` is presence-tolerant). One arm per kind of staged object (a plain blob, a
/// chunked file's recipe, one of its chunks), and one for `park`'s push.
///
/// Each arm asserts the `staged_content_missing` refusal (exit 25) names the path and the missing
/// hash and moves nothing; that loading again alone does not clear it (the stat cache proves the
/// file unchanged and stores nothing, so the fixture really is in the state the named remedy is
/// for); and that the named remedy, `restore --staged` then `load`, lets the verb through with the
/// file restorable afterwards. The blob arm's file starts with `-`, so the remedy as printed (the
/// path after `--`) is what gets run.
///
/// Red with the `require_staged_content_present` call removed from `stack_parcel` (every stack arm
/// goes through) or from `park` (the park arm does), and with its chunk check removed (the chunk
/// arm does).
#[test]
fn stack_and_park_refuse_a_staged_object_the_store_lost() {
    let mut failures = Vec::new();

    for arm in ["blob", "recipe", "chunk", "park"] {
        let warehouse = Warehouse::new(&format!("lost-staged-{}", arm));
        let first = warehouse.stack("app.txt", "v1\n", "first");

        let (file, content) = match arm {
            "recipe" | "chunk" => ("big.bin", chunkable_content()),
            "blob" => ("-wip.txt", b"work in progress\n".to_vec()),
            _ => ("wip.txt", b"work in progress\n".to_vec()),
        };
        let staged = load_and_name_staged(&warehouse, file, &content);

        let missing = match arm {
            "recipe" => staged.iter()
                .find(|hash| warehouse.scoped(|| forklift_core::util::object_utils::load_recipe(hash).is_ok()))
                .cloned(),
            "chunk" => staged.iter()
                .find(|hash| warehouse.scoped(|| forklift_core::util::object_utils::load_recipe(hash).is_err()))
                .cloned(),
            _ => staged.first().cloned(),
        };
        let Some(missing) = missing else {
            failures.push(format!("{}: fixture did not stage the object to lose; load created {:?}", arm, staged));
            continue;
        };
        if file != "big.bin" && staged.len() != 1 || file == "big.bin" && staged.len() < 3 {
            failures.push(format!("{}: unexpected staged objects {:?}", arm, staged));
            continue;
        }

        warehouse.delete_object(&missing);
        let verb: &[&str] = if arm == "park" { &["park"] } else { &["stack", "second"] };

        let refused = warehouse.run(verb);
        let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
        if refused.status.code() != Some(25) || !stderr.contains(file) || !stderr.contains(&missing) {
            failures.push(format!(
                "{}: `{}` must refuse with exit 25 naming {} and {}; got {:?}: {}",
                arm, verb.join(" "), file, missing, refused.status.code(), stderr
            ));
        }
        if warehouse.head("main") != first {
            failures.push(format!("{}: the pallet moved past {}", arm, first));
            continue;
        }
        if std::fs::read(warehouse.root.join(file)).ok().as_deref() != Some(&content[..]) {
            failures.push(format!("{}: the refused `{}` disturbed the working copy", arm, verb.join(" ")));
            continue;
        }

        // Loading again alone stores nothing: the stat data still proves the file unchanged.
        warehouse.run_ok(&["load", "."]);
        if warehouse.run(verb).status.code() != Some(25) {
            failures.push(format!("{}: a plain reload cleared the loss, so the stat cache never hit", arm));
        }

        // The remedy the refusal names, as printed.
        let remedy = if file.starts_with('-') { format!("-- {}", file) } else { file.to_string() };
        let printed = format!("\"forklift restore --staged {}\", then \"forklift load {}\"", remedy, remedy);
        if !stderr.contains(&printed) {
            failures.push(format!("{}: the refusal must name the remedy {}; got: {}", arm, printed, stderr));
        }
        let unstaged = warehouse.run(&["restore", "--staged", "--", file]);
        let reloaded = warehouse.run(&["load", "--", file]);
        let again = warehouse.run(verb);
        if !unstaged.status.success() || !reloaded.status.success() || !again.status.success() {
            failures.push(format!(
                "{}: `restore --staged` then `load` must let `{}` through; restore: {} / load: {} / {}",
                arm, verb.join(" "), String::from_utf8_lossy(&unstaged.stderr),
                String::from_utf8_lossy(&reloaded.stderr), String::from_utf8_lossy(&again.stderr)
            ));
            continue;
        }

        if arm == "park" {
            warehouse.run_ok(&["park", "pop"]);
        } else {
            std::fs::remove_file(warehouse.root.join(file)).unwrap();
            warehouse.run_ok(&["restore", "--", file]);
        }
        if std::fs::read(warehouse.root.join(file)).ok().as_deref() != Some(&content[..]) {
            failures.push(format!("{}: the file did not come back after `{}`", arm, verb.join(" ")));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// FORK-157 — the check covers staged work only. A file the head already holds, unchanged, is not
/// this bay's to vouch for: losing its object must not block a stack of something else, since
/// neither remedy could clear it (`restore --staged` resets to the same lost hash). The control:
/// the same loss on the staged file itself does refuse. Red with the head comparison removed from
/// `require_staged_content_present` (the head file's loss refuses the stack).
#[test]
fn stack_ignores_a_lost_object_the_head_already_names() {
    let warehouse = Warehouse::new("lost-head-blob");
    let before = warehouse.loose_objects();
    warehouse.stack("app.txt", "v1\n", "first");
    let app_blob = warehouse.scoped(|| {
        let mut created: Vec<String> = warehouse.loose_objects().difference(&before).cloned().collect();
        created.retain(|hash| forklift_core::util::object_utils::load_blob(hash).is_ok_and(|blob| blob.content == b"v1\n"));
        created
    });
    assert_eq!(app_blob.len(), 1, "the head's blob for app.txt: {:?}", app_blob);

    let staged = load_and_name_staged(&warehouse, "wip.txt", b"work in progress\n");
    warehouse.delete_object(&app_blob[0]);

    let stacked = warehouse.run(&["stack", "second"]);
    assert!(stacked.status.success(), "a lost head-only object blocked the stack: {}", String::from_utf8_lossy(&stacked.stderr));

    // Control: the same loss on staged work refuses.
    let mut more = load_and_name_staged(&warehouse, "more.txt", b"more\n");
    more.retain(|hash| warehouse.scoped(|| {
        forklift_core::util::object_utils::load_blob(hash).is_ok_and(|blob| blob.content == b"more\n")
    }));
    assert_eq!(more.len(), 1, "staged {:?} then {:?}", staged, more);
    warehouse.delete_object(&more[0]);
    assert_eq!(warehouse.run(&["stack", "third"]).status.code(), Some(25));
}

/// FORK-157 — damage the remedy cannot repair is not reported as lost staged content. A head tree
/// that cannot be read would otherwise leave every file beneath it looking staged (the staged file
/// sits inside it, since an unchanged subtree is pruned by hash and never read); a recipe that is
/// present but unreadable would be "re-stored" by a load that skips writing a hash already there.
/// Each fails the stack with an error naming the damaged object, not exit 25, and a next step that
/// works where one exists: `restore --staged` on the file with the damaged recipe lets the rest
/// stack (no command repairs a damaged head tree, so that error only names it). Red with the head
/// tree read made tolerant again (the head arm stacks over the damage without a word), and with an
/// unreadable recipe reported as missing (the recipe arm refuses with exit 25 and the false remedy).
#[test]
fn stack_names_an_unreadable_head_tree_or_recipe_as_itself() {
    let mut failures = Vec::new();

    for arm in ["head-tree", "recipe"] {
        let warehouse = Warehouse::new(&format!("unreadable-{}", arm));
        let before = warehouse.loose_objects();
        warehouse.stack("lib/x.txt", "x\n", "first");
        let first_objects: Vec<String> = warehouse.loose_objects().difference(&before).cloned().collect();

        let (damaged, named) = if arm == "head-tree" {
            let lib_tree = warehouse.scoped(|| first_objects.iter().find(|hash| {
                forklift_core::util::object_utils::load_tree(hash).is_ok_and(|tree| tree.get_file("x.txt").is_some())
            }).cloned());
            let Some(lib_tree) = lib_tree else {
                failures.push(format!("{}: no tree holding x.txt among {:?}", arm, first_objects));
                continue;
            };
            load_and_name_staged(&warehouse, "lib/wip.txt", b"work in progress\n");
            (lib_tree, "for \"lib\"")
        } else {
            let staged = load_and_name_staged(&warehouse, "big.bin", &chunkable_content());
            let recipe = staged.iter()
                .find(|hash| warehouse.scoped(|| forklift_core::util::object_utils::load_recipe(hash).is_ok()))
                .cloned();
            let Some(recipe) = recipe else {
                failures.push(format!("{}: no recipe among {:?}", arm, staged));
                continue;
            };
            (recipe, "\"big.bin\" is staged, but its recipe")
        };

        let path = warehouse.root.join(".forklift").join("objects").join(&damaged[..2]).join(&damaged[2..]);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"not an object").unwrap();

        let stacked = warehouse.run(&["stack", "second"]);
        let stderr = String::from_utf8_lossy(&stacked.stderr).to_string();
        if stacked.status.success() || stacked.status.code() == Some(25) || !stderr.contains(&damaged) || !stderr.contains(named) {
            failures.push(format!(
                "{}: the stack must fail naming {} and {}, not as lost staged content; got {:?}: {}",
                arm, named, damaged, stacked.status.code(), stderr
            ));
            continue;
        }

        if arm == "recipe" {
            std::fs::write(warehouse.root.join("other.txt"), b"other\n").unwrap();
            let unstaged = warehouse.run(&["restore", "--staged", "big.bin"]);
            let loaded = warehouse.run(&["load", "other.txt"]);
            let again = warehouse.run(&["stack", "the rest"]);
            if !unstaged.status.success() || !loaded.status.success() || !again.status.success() {
                failures.push(format!(
                    "{}: `restore --staged big.bin` must let the rest stack; restore: {} / load: {} / stack: {}",
                    arm, String::from_utf8_lossy(&unstaged.stderr), String::from_utf8_lossy(&loaded.stderr),
                    String::from_utf8_lossy(&again.stderr)
                ));
            }
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
