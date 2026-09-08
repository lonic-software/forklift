//! Executed evidence for the **collected-referent class**: a durable hash pin whose referent has
//! been garbage-collected.
//!
//! `gc_utils::collect_live_set` roots pallet refs, bay-scoped parcels and every trust-pin root
//! (`office_utils::collect_trust_pin_roots`: the anchor's `adopts` pin, its `boundary` snapshot,
//! and every key's revocation `distrust_boundary`). Several other places
//! still write a parcel or object hash durably with no such root, and read it back regardless.
//! When gc collects an unrooted referent, each reader fails differently — and the members
//! disagree about what correct behaviour even is, which is why the pallet-lifecycle design
//! replaced its class invariant with a per-member ledger.
//!
//! | member | reader kind | leg here |
//! | --- | --- | --- |
//! | revocation `distrust_boundary` | verifying | **FIXED (FORK-81)** — rooted; the two `a_boundary_pin_*`/`a_distrust_boundary_pin_*` legs (FORK-63/64 spike) |
//! | `audit`'s boundary head | verifying | **FIXED (FORK-81)** — rooted; `an_undone_boundary_head_*`, `a_boundary_head_and_the_legacy_parcel_*`. The residual gap a *rooted* pin can still reach — never fetched, or genuinely lost, since gc can no longer produce it — is a separate finding with its own legs (not gc-driven, no canary): `an_absent_interior_ancestor_behind_a_present_boundary_head_is_named_not_accused_of_tampering`, `a_genuinely_pre_trust_side_pallet_parcel_behind_an_absent_boundary_head_refuses_instead_of_accusing`, `the_accusation_still_fires_with_no_gap_at_all`. **PR #121 round 1 narrowed the tolerance to interior-only gaps; round 2 reverted that narrowing** — an absent boundary HEAD, exactly like an absent interior ancestor, un-resolves the boundary walk's negative for anything the present portion cannot itself settle, because there is no such thing as an "unrelated" absent reference: relatedness to the parcel under test is precisely the question the walk exists to answer (see `audit_utils::verify_pallet_history`'s `boundary_gap` doc). Reproduced end to end against a real origin server, from ordinary commands, in `tests/remote.rs`'s two Construction T/D legs. |
//! | `CherryPickState.source` | **acting** | `a_collected_cherry_pick_source_*` (FORK-82) |
//! | `Tag.subject` under a torn taint | **healing** | `a_torn_taint_over_an_absent_tag_subject_*` (FORK-83) |
//! | staged inventory shard under a torn taint | **healing** | `staging_a_file_and_then_collecting_*` (FORK-83, widened) |
//!
//! `Tag.subject`'s *rendering* reader is pinned separately in `tag_subject_gc.rs` (FORK-79).
//!
//! **Four legs carry a canary** — a parcel pinned by nothing — and assert it was collected, so
//! "the pin survived" can never be satisfied by a collector that swept nothing. The canary check
//! holds both before and after any fix; it is the legs' own assertions that invert. (**Seven** of
//! the eight legs drive collection — six call `collect_garbage`, and
//! `an_undone_boundary_head_survives_the_sweep_and_audit_passes` drives the same sweep through
//! `compact --all` — so "gc-driven" is not the dividing line. Only the torn-taint control collects
//! nothing.)
//!
//! The other four discriminate differently: the two anchor legs now assert `b_present &&
//! legacy_present && audit succeeds` (FORK-81 rooted the boundary pin, so both survive together;
//! before the fix this was `!b_present && legacy_present`, already two-sided in the other
//! direction); `a_torn_taint_alone_...` is a matched companion to the wedge leg rather than a
//! canary user; and `staging_a_file_...` names the exact blob it staged and asserts that specific
//! object was collected, which is a canary's job done directly.
//!
//! **Three further legs, added for FORK-81's second finding, sit outside this counting
//! entirely.** They delete an object directly rather than driving `collect_garbage` — the state
//! they construct (a rooted pin's referent simply never held locally, or genuinely lost) is not
//! something gc can produce any more, so there is no sweep for a canary to witness and none is
//! used. See
//! `an_absent_interior_ancestor_behind_a_present_boundary_head_is_named_not_accused_of_tampering`,
//! `a_genuinely_pre_trust_side_pallet_parcel_behind_an_absent_boundary_head_refuses_instead_of_accusing`
//! and `the_accusation_still_fires_with_no_gap_at_all`.

use std::path::PathBuf;
use std::process::{Command, Output};

use forklift_core::globals::StorageRootScope;
use forklift_core::util::{audit_utils, file_utils, gc_utils, office_utils};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

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
        warehouse.run_ok(&["config", "operator.name", "spike@forklift"]);
        warehouse.run_ok(&["config", "operator.identifier", "spike@forklift"]);

        warehouse
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(FORKLIFT)
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
        std::fs::read_to_string(self.root.join(".forklift").join("pallet"))
            .unwrap()
            .trim()
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

/// FORK-82 — THE ACTING READER. `CherryPickState.source` is a durable parcel hash with no GC
/// root, and unlike the rendering members of this class its reader *acts* on the hash rather
/// than printing it.
///
/// The pin: `CherryPickState.source` (`cherry_pick_utils.rs:33`) is written to disk by
/// `write_state` (`:88-90`). It is not rooted — `gc_utils::collect_live_set` roots pallet heads,
/// bay-scoped parcels and every trust-pin root (`anchor.adopts`, its `boundary` snapshot, and
/// every key's `distrust_boundary` — `office_utils::collect_trust_pin_roots`), and a `grep` for
/// "cherry" in `gc_utils.rs`/`bay_utils.rs`/`office_utils.rs` returns nothing.
///
/// The reader: the completing `stack` calls `collect_source_authors` (`stack_utils.rs:233`),
/// which calls `object_utils::load_parcel(source)?` (`cherry_pick_utils.rs:120`) and propagates
/// whatever that returns.
///
/// The ticket reached this by reading and explicitly owed an execution. This is it. Note the
/// orphaning uses `undo`, not `delete_pallet_ref` — so, as with the anchor leg above, this needs
/// no deletion verb and is live today rather than a hazard the verb would introduce.
#[test]
fn a_collected_cherry_pick_source_makes_the_completing_stack_fail_unattributed() {
    let warehouse = Warehouse::new("pick");

    warehouse.stack("app.txt", "base\n", "base");

    // The canary, as in every other leg: pinned by nothing, so its collection witnesses that the
    // sweep actually ran rather than bailing early.
    warehouse.run_ok(&["palletize", "canary"]);
    let canary = warehouse.stack("canary.txt", "c1\n", "canary work");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.delete_pallet_ref("canary");

    // The source, on its own pallet, touching the file `main` is about to touch so the pick
    // conflicts and a state file is written.
    warehouse.run_ok(&["palletize", "feature"]);
    let source = warehouse.stack("app.txt", "from-feature\n", "feature work");

    // Orphan the source with a shipped command. `undo` moves the head back to `pre_head`
    // (`journal_utils.rs:191`), so nothing reachable from a ref names `source` — but the object
    // is still on disk, so the pick below can still resolve it. No shift between the stack and
    // the undo: `shift` is journaled, and one here would make `undo` revert the shift instead.
    warehouse.run_ok(&["undo"]);

    // `undo` is soft: the head moves back but the feature content stays in the working tree, so
    // `shift` would refuse over local changes. Unstage, then discard, so the shift is clean.
    warehouse.run_ok(&["restore", "--staged", "."]);
    warehouse.run_ok(&["restore", "."]);
    warehouse.run_ok(&["shift", "main"]);

    warehouse.stack("app.txt", "from-main\n", "main work");

    // 1. The pick conflicts, so the state file is written and the pick is left in progress.
    let pick = warehouse.run(&["cherry-pick", &source]);
    println!(
        "PICK: `cherry-pick` exit = {:?}\n  stdout: {}\n  stderr: {}",
        pick.status.code(),
        String::from_utf8_lossy(&pick.stdout).trim(),
        String::from_utf8_lossy(&pick.stderr).trim()
    );

    let state_file = warehouse.root.join(".forklift").join("cherry-pick");
    println!("PICK: state file present = {}", state_file.exists());

    assert!(
        state_file.exists(),
        "the pick must be left in progress with its state on disk — that durable `source` hash is \
         the pin under test. If this fails the pick did not conflict and the fixture is wrong."
    );

    // 2. Collect. Nothing roots the source.
    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    let present = warehouse.scoped(|| file_utils::does_object_exist(&source).unwrap());

    println!(
        "PICK: gc deleted {} object(s); source {} present = {}",
        stats.deleted, source, present
    );

    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(
        !present,
        "the pick source survived collection, so this leg is not testing what it says. The \
         fixture knows which of the two causes that is; do not make the assertion below guess."
    );

    // 3. Resolve the conflict and complete the pick, which is where the acting read happens.
    std::fs::write(warehouse.root.join("app.txt"), "resolved\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    let completing = warehouse.run(&["stack", "completed pick"]);

    let err = String::from_utf8_lossy(&completing.stderr).to_string();

    println!(
        "PICK: completing `stack` exit = {:?}\n  stdout: {}\n  stderr: {}",
        completing.status.code(),
        String::from_utf8_lossy(&completing.stdout).trim(),
        err.trim()
    );

    // THE DEFECT, asserted rather than printed. FOUR claims, and the inversion map matters
    // because the candidate fixes invert different subsets:
    //
    //   * root the pick source     -> `stack` SUCCEEDS. Claims 1 and 2 invert. Claims 3 and 4
    //                                 do NOT: stderr is empty, so both negative substring tests
    //                                 stay trivially true. That is why they are gated below.
    //   * presence check + remedy  -> `stack` still fails. Claim 1 holds; claim 2 ALSO inverts,
    //                                 because a presence check is precisely what replaces the
    //                                 raw propagated string; claims 3/4 invert.
    //   * abort path only          -> `stack` still fails; claims 3 AND 4 invert. Not just 4:
    //                                 the abort text the code already prints is `or remove
    //                                 ".forklift/cherry-pick" to abort it`
    //                                 (`commands/cherry_pick.rs:224`), and that PATH contains the
    //                                 substring "cherry-pick", so an error advertising the abort
    //                                 BY THAT PATH also trips the attribution claim. Not every
    //                                 possible wording does: "remove the in-progress pick state"
    //                                 would invert claim 4 alone.
    assert!(
        !completing.status.success(),
        "the completing `stack` succeeded, so the source survived collection — either the pick \
         source became a GC root (FORK-82 fixed by rooting) or the fixture stopped orphaning it"
    );
    assert!(
        err.contains("Error while reading object from file"),
        "expected the raw object-read error `load_parcel` propagates from \
         `collect_source_authors` (cherry_pick_utils.rs:120); got: {}",
        err
    );

    // Claims 3 and 4 are about the CONTENT of a failure, so they are only meaningful while there
    // is one. What makes them non-trivial is assertion 2 above, not any guard here: in a world
    // where `stack` succeeds, stderr is empty and both negatives hold vacuously — but assertions
    // 1 and 2 have already failed by then, so the leg is red for the right reason regardless.
    // (Round 1 added an `err.is_empty()` gate here for this; it was unreachable for exactly that
    // reason and has been removed rather than left as reassuring dead weight.)
    assert!(
        !err.to_lowercase().contains("cherry-pick"),
        "the error now names the cherry-pick, so it is no longer unattributed — invert this leg. \
         stderr: {}",
        err
    );
    assert!(
        !err.to_lowercase().contains("abort"),
        "the completing error now advertises the abort path — invert this leg. Note the narrow \
         claim: an abort path EXISTS and `cherry-pick` itself printed it (\"or remove \
         \\\".forklift/cherry-pick\\\" to abort it\"). The defect is that the error the operator \
         actually hits, possibly days later, does not repeat it. stderr: {}",
        err
    );
}

/// FORK-83 — THE HEALING READER, and the only member of this class that takes the whole
/// warehouse down.
///
/// `recovery_utils::collect_walk_roots` roots every tag's subject (`recovery_utils.rs:1277-1279`)
/// — deliberately, because heal's walk must be a superset of gc's live set. The torn-taint rescan
/// builds its roots from that walk (`:1418-1419`) and folds every referenced-but-absent hash into
/// the remainder (`:953`, `:981-985`); a non-empty remainder returns
/// `Err(torn_rescan_dangling_refusal(...))` (`:1024`), which is a store-wide refusal rather than
/// one command's exit code.
///
/// So an absent tag subject plus a torn taint bricks the store. And it cannot be cleared:
/// `TagAction` is `Create`/`Show`/`List` only (`cli.rs:1011-1038`) — no delete, names immutable —
/// and `heal` cannot restore an object nobody holds.
///
/// The taint is written directly rather than driven through a command because a torn taint is by
/// definition crash debris: `parse_taint_content` (`taint_utils.rs:623`) calls a file torn when it
/// lacks the `END\n` suffix, which is exactly what a crash mid-write leaves. Everything else here
/// is a shipped command. Note `crates/forklift/src/main.rs:60` calls `taint_utils::activate()`, so
/// the CLI really does read these files.
#[test]
fn a_torn_taint_over_an_absent_tag_subject_wedges_the_whole_warehouse() {
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

    // ISOLATE THE TAG. A soft `undo` leaves the undone content STAGED, and heal's walk roots
    // staged inventory shards (`recovery_utils.rs:1224-1233`) — so without this the leg wedges
    // for two independent reasons and three of its four claims below could not invert on a
    // tag-subject fix. Unstaging drops the shard's reference, leaving the tag subject as the
    // only dangling root. The staged-shard half gets its own leg further down; it is a wider
    // defect, not this one.
    warehouse.run_ok(&["restore", "--staged", "."]);

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    let present = warehouse.scoped(|| file_utils::does_object_exist(&tagged).unwrap());

    println!(
        "TORN: gc deleted {} object(s); tag subject {} present = {}\n      canary was {}",
        stats.deleted, tagged, present, canary
    );
    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(!present, "the fixture needs the tag subject collected");

    // The torn taint: a parseable prefix with no `END\n` suffix, which is what a crash
    // mid-write-of-a-line leaves behind.
    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"objects/ab/cdef\n").unwrap();

    let healed = warehouse.run(&["heal"]);
    println!(
        "TORN: `heal` exit = {:?}\n  stdout: {}\n  stderr: {}",
        healed.status.code(),
        String::from_utf8_lossy(&healed.stdout).trim(),
        String::from_utf8_lossy(&healed.stderr).trim()
    );

    // An ordinary command, to show the refusal is store-wide rather than heal's own.
    let ordinary = warehouse.run(&["stocktake"]);
    println!(
        "TORN: `stocktake` exit = {:?}\n  stdout: {}\n  stderr: {}",
        ordinary.status.code(),
        String::from_utf8_lossy(&ordinary.stdout).trim(),
        String::from_utf8_lossy(&ordinary.stderr).trim()
    );

    // And there is no in-tool exit: no way to retire the tag whose subject is the blocker.
    let tag_help = warehouse.run(&["tag", "--help"]);
    let help = String::from_utf8_lossy(&tag_help.stdout).to_string();

    // THE WEDGE, asserted. The four claims are: heal exits 21, the refusal names `tagged`,
    // an ordinary command is refused too, and the remedy is circular. (The `tag` subcommand
    // assertion further down is NOT one of them — it pins the missing in-tool exit, and being an
    // exact-set equality it reddens on ANY change to the subcommand list: a retire verb, but also
    // an unrelated addition, a rename, or a reordering of `TagAction`'s variants, since clap
    // prints declaration order. Never on a rescan change. The isolation assertion BELOW is
    // likewise a property of the fixture, not a claim about the defect.)
    //
    // It is worth being exact about how they invert, because an earlier version of this comment
    // was not.
    //
    // Two of the assertions above (`assert_the_sweep_ran`, `!present`) are FIXTURE PRECONDITIONS,
    // not claims: they say the store really reached the state under test. A mutation that stops
    // the subject being collected reddens THERE, which is correct and is what happens if the fix
    // chosen is to root tag subjects — a remedy FORK-83 explicitly rules out.
    //
    // The four claims below invert on the fixes actually on the table, all of which change what
    // the RESCAN does with a referenced-absent subject while leaving it collected.
    //
    // Two companion legs, and they support DIFFERENT things. This sentence has now been wrong
    // twice, in opposite directions, so it is worth spelling out which is which:
    //
    //   * `a_torn_taint_alone_...` (the control) is the only leg that varies DANGLING-NESS: torn
    //     taint, nothing dangling, exit 0. Paired with this leg's exit 21 it is what separates the
    //     dangling reference from torn-ness — the contingency asserted here. It is a weak pair
    //     (it differs in several variables at once, which is why it cannot show anything about the
    //     TAG specifically), but on this one axis it is the only evidence there is.
    //   * `staging_a_file_and_then_collecting_...` holds dangling-ness CONSTANT and varies the tag
    //     and the `undo`. It therefore shows the wedge is not tag-specific, and it cannot speak to
    //     torn-ness at all.
    //
    // Round 1 corrected the control's doc for claiming the second of these; round 2 then demoted
    // the control out of the first, which it does support. Both halves are stated above.
    let heal_err = String::from_utf8_lossy(&healed.stderr).to_string();
    let ordinary_err = String::from_utf8_lossy(&ordinary.stderr).to_string();

    assert_eq!(
        healed.status.code(), Some(21),
        "`heal` must refuse with the durability-taint exit; got {:?}, stderr: {}",
        healed.status.code(), heal_err
    );
    assert!(
        heal_err.contains(&tagged),
        "the refusal must name the collected tag subject {} as dangling — that is the whole \
         finding. stderr: {}",
        tagged, heal_err
    );
    // THE ISOLATION ITSELF, pinned. The unstage above exists so the tag subject is the SOLE
    // dangling reference — that is what restores invertibility to the claims below. Asserting
    // only that `tagged` appears would stay green if a future change re-introduced a second
    // dangling root, silently losing the property round 1 was fixed to gain.
    // "and 1 reference(s)", not "1 reference(s)": the latter is a substring of "11 reference(s)"
    // and "101 reference(s)", so it would go green in exactly the case it exists to catch — a
    // change that re-roots a whole class and pushes the remainder into double digits.
    assert!(
        heal_err.contains("and 1 reference(s)"),
        "the tag subject must be the ONLY dangling reference; more than one means the unstage no \
         longer isolates it and three of the four claims below stop inverting. stderr: {}",
        heal_err
    );
    assert_eq!(
        ordinary.status.code(), Some(21),
        "the refusal must be STORE-WIDE, not heal's own exit code: an ordinary `stocktake` has \
         to be refused too. got {:?}, stderr: {}",
        ordinary.status.code(), ordinary_err
    );

    // No in-tool exit. If a retire/delete verb ever lands, this is the assertion to invert —
    // and the wedge stops being a wedge.
    // Matched against the SUBCOMMAND LIST only, not the whole help text: a bare substring sweep
    // over `help` also fires on a reworded description or an unrelated `--remove-*` option on a
    // sibling, producing a red with nothing to do with FORK-83.
    let subcommands: Vec<String> = help
        .lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        // Exactly two leading spaces then a non-space. Clap wraps a long description onto a
        // continuation line indented far deeper (`forklift --help` renders one as
        // `               bl]`), which also starts with two spaces — so the looser test emits
        // "bl]" as a subcommand and reddens the equality below with the wrong explanation.
        .take_while(|line| line.starts_with("  ") && !line.starts_with("   "))
        .map(|line| line.trim().split_whitespace().next().unwrap_or("").to_string())
        .collect();

    println!("STAGED/TORN: `tag` subcommands = {:?}", subcommands);
    assert!(
        !subcommands.is_empty(),
        "failed to parse the subcommand list out of `tag --help`; the parse, not the claim, is \
         what broke. help was:\n{}",
        help
    );
    // EXACT SET, not a blocklist. Round 1 narrowed an over-broad substring sweep to equality
    // against delete/retire/remove — which is under-broad in the other direction: `rm`, `drop`,
    // `forget` or `revoke` would give the wedge an in-tool exit while this stayed green. The
    // exact set is narrow AND total: any change to the list reddens here and gets read. It needs
    // a manual edit when the list legitimately changes, which is the intended cost.
    assert_eq!(
        subcommands, vec!["create", "show", "list"],
        "`tag`'s subcommand list changed. If a retire/delete verb landed, the wedge now has an \
         in-tool exit — invert this leg and revisit FORK-83's premise. If the change is unrelated, \
         update the expected set."
    );

    // AND THE REMEDY IS CIRCULAR — the part the ticket does not state. The refusal that blocks
    // every command tells the operator to run the one command that just refused.
    assert!(
        ordinary_err.contains("forklift heal"),
        "expected the store-wide refusal to direct the operator to `forklift heal`; stderr: {}",
        ordinary_err
    );
}

/// CONTROL: a torn taint with nothing dangling heals cleanly.
///
/// **This is a narrow control and it is worth being exact about what it establishes**, because an
/// earlier version of this comment claimed much more. It removes the tag, the `undo` AND the
/// collection, so its green shows only that torn-ness *alone* does not refuse — which is what
/// stops the wedge leg above from being satisfied by "a torn taint bricks any warehouse".
///
/// It does **not** show the tag is load-bearing, and an earlier draft that said so was wrong:
/// `staging_a_file_and_then_collecting_wedges_the_warehouse_with_no_tag_involved` below refutes
/// it outright. That draft also reasoned that the wedge leg's second dangling hash "is
/// byte-identical across runs, so it is a fixed-content object rather than anything this fixture
/// created" — a non-sequitur, since a fixture with hardcoded file bodies produces fixed content
/// by construction. It was this fixture's own staged blob, and chasing it is what turned up the
/// wider defect.
///
/// The tag-only control is now inside the wedge leg itself: it unstages after the `undo`, so the
/// tag subject is the only dangling root there.
#[test]
fn a_torn_taint_alone_on_an_untouched_warehouse_is_the_control() {
    let warehouse = Warehouse::new("torn-control");

    warehouse.stack("app.txt", "v1\n", "first");

    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"objects/ab/cdef\n").unwrap();

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

    // The distinguishing half. Same torn taint, same shipped commands, no absent tag subject:
    // the store heals and stays usable. Without this leg the wedge leg above is satisfied just
    // as well by "a torn taint bricks any warehouse", which is false.
    assert!(
        healed.status.success(),
        "a torn taint alone must heal cleanly — if this refuses, the wedge is not about tag \
         subjects at all and FORK-83 is filed too narrowly. stdout: {} / stderr: {}",
        String::from_utf8_lossy(&healed.stdout), String::from_utf8_lossy(&healed.stderr)
    );
    assert!(
        ordinary.status.success(),
        "with the taint cleared, ordinary commands must work again; stderr: {}",
        String::from_utf8_lossy(&ordinary.stderr)
    );
}

/// FORK-83 IS FILED TOO NARROWLY — the wedge needs no tag, and this is the minimal case.
///
/// `collect_walk_roots` roots **every bay's staged inventory shards** and records, in terms, that
/// gc "deliberately does not root" them — "an unstacked staged shard is a pre-existing, accepted
/// gc design choice, not a bug this walk needs to match" (`recovery_utils.rs:1224-1233`).
///
/// That asymmetry was accepted for the *walk*, where its only effect is a conservative extra
/// root. Nobody re-checked it against the **torn rescan**, which folds every referenced-but-absent
/// hash into a remainder and turns a non-empty remainder into a store-wide refusal
/// (`recovery_utils.rs:953`, `:981-985`, `:1024`). Under the rescan, the same asymmetry bricks
/// the warehouse.
///
/// So the reachable sequence is not "tag, undo, collect" — it is:
///
/// 1. `load` a file and do not stack it yet (ordinary work in progress).
/// 2. Collection runs (`maintenance: auto on`, or any `forklift compact`).
/// 3. A crash tears the taint record.
///
/// The tag-subject leg above is one instance of this class. The `undo` path reaches it too, for
/// the same reason: a soft `undo` leaves the undone content staged.
///
/// **And the remedy is inside the wedge.** Unlike a tag, a staged shard *can* be dropped —
/// `restore --staged` is exactly the command that would clear the dangling reference — but
/// entry-heal refuses every command but `heal`/`audit`, so it cannot be run. `heal` refuses, and
/// `audit` reports the warehouse clean.
#[test]
fn staging_a_file_and_then_collecting_wedges_the_warehouse_with_no_tag_involved() {
    let warehouse = Warehouse::new("staged");

    warehouse.stack("app.txt", "v1\n", "first");

    // Stage a new file and never stack it — the ordinary work-in-progress state. Snapshot the
    // store around the `load` so the staged blob can be named: without that, the assertions below
    // would accept a refusal about ANY object and the leg would stop being about staged shards.
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
    // Precondition, not a claim — see the wedge leg's note. Names the blob rather than counting
    // deletions: `stats.deleted > 0` passes on ANY garbage, so in a world where gc started
    // rooting staged shards this leg would sail past here and die at the `exit 21` assertion
    // below, whose message blames the rescan — the wrong cause. It works today only because this
    // fixture happens to have exactly one piece of garbage.
    assert!(
        !warehouse.scoped(|| file_utils::does_object_exist(&wip_blob).unwrap()),
        "gc must collect the staged-but-unstacked blob {} for this fixture to mean anything — if \
         it stopped doing so, the gc/walk asymmetry is gone and this whole leg inverts \
         (gc deleted {} object(s))",
        wip_blob, stats.deleted
    );

    // The torn taint: crash debris, per `parse_taint_content` (taint_utils.rs:623).
    let taint_dir = warehouse.root.join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"objects/ab/cdef\n").unwrap();

    let healed = warehouse.run(&["heal"]);
    let heal_err = String::from_utf8_lossy(&healed.stderr).to_string();
    println!("STAGED: heal exit = {:?}\n  stderr: {}", healed.status.code(), heal_err.trim());

    assert_eq!(
        healed.status.code(), Some(21),
        "`heal` must refuse over the stranded staged blob, with NO tag anywhere in this fixture. \
         If this passes, the rescan stopped folding staged-shard references into the remainder. \
         stderr: {}",
        heal_err
    );
    assert!(
        heal_err.contains("genuinely dangling"),
        "expected the torn-rescan dangling refusal; stderr: {}", heal_err
    );
    assert!(
        heal_err.contains(&wip_blob),
        "the refusal must name the STAGED BLOB {} specifically — that a refusal happened at all \
         is not this leg's claim, and would still pass if some unrelated object dangled while \
         staged shards stopped being rooted. stderr: {}",
        wip_blob, heal_err
    );

    // The refusal is store-wide, and — the part that makes it a wedge rather than an error — it
    // also blocks `restore --staged`, the one command that would clear the dangling reference.
    for attempt in [
        vec!["stocktake"],
        vec!["restore", "--staged", "."],
        vec!["restore", "."],
    ] {
        let out = warehouse.run(&attempt);
        println!("STAGED: `{}` exit = {:?}", attempt.join(" "), out.status.code());
        assert_eq!(
            out.status.code(), Some(21),
            "`{}` must be refused by entry-heal — it is `restore --staged` being unreachable that \
             makes this unescapable. If this one succeeds, the wedge has an in-tool exit and this \
             leg inverts. stderr: {}",
            attempt.join(" "), String::from_utf8_lossy(&out.stderr)
        );
    }

    // The contradiction worth keeping: the one non-heal command that IS allowed reports the
    // warehouse healthy, while every other command refuses it as tainted.
    let audit = warehouse.run(&["audit"]);
    println!("STAGED: `audit` exit = {:?}", audit.status.code());
    assert!(
        audit.status.success(),
        "`audit` is expected to report this warehouse CLEAN while every other command refuses it \
         as tainted — two shipped commands giving opposite verdicts on one store. If audit starts \
         refusing too, invert this leg. stderr: {}",
        String::from_utf8_lossy(&audit.stderr)
    );
}
