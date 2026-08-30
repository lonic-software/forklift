//! SPIKE for the pallet-lifecycle design (FORK-63/FORK-64): is a revocation's distrust
//! boundary a GC root?
//!
//! A revocation snapshots pallet heads into `distrust_boundary` (`office.rs:526-557`), but
//! `gc_utils::collect_live_set` roots only pallet refs, bay-scoped parcels and the anchor's
//! `adopts` pin (`gc_utils.rs:136-171`). Today those pins survive only because nothing ever
//! deletes a ref. This test builds the state a pallet-deletion verb would create — a boundary
//! pin whose ref is gone — and asks what the collector actually does with it.

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

/// THE DEFECT. Unlink the ref — exactly what a pallet-deletion verb would do — and the same
/// `gc` collects a hash that a signed revocation still names, leaving the boundary permanently
/// unresolvable: the parcels that exculpated the revoked key's signatures are gone.
///
/// Green today, and it is the two `!` assertions that must be **inverted** when boundary pins
/// become GC roots. Reverting that fix reddens the inverted form; that is the pairing. The
/// canary check does NOT invert — it holds in both worlds, which is the point of it.
#[test]
fn a_boundary_pin_is_collected_once_its_pallet_ref_is_deleted() {
    let (warehouse, key, side_head, canary) = warehouse_with_a_pinned_side_head("deleted");

    warehouse.delete_pallet_ref("side");

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    let (present, resolvable) = present_and_resolvable(&warehouse, &key, &side_head);

    println!(
        "DEFECT: gc deleted {} object(s); pin present = {}; boundary resolvable = {}",
        stats.deleted, present, resolvable
    );

    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(!present, "the pin was collected once its ref was deleted");
    assert!(!resolvable, "and the boundary is now unresolvable");
}

/// IS THE ANCHOR-`boundary` FALSE-TAMPERING STATE REACHABLE WITH SHIPPED COMMANDS ONLY?
///
/// The design claimed it needs a ref unlink, i.e. that it is a hazard the not-yet-built deletion verb
/// introduces rather than a live defect. Review disagreed: `undo` moves a head *backwards*
/// (`journal_utils.rs:191`) — the same move the tag leg uses — which orphans the boundary head with no
/// unlink at all, and `compact --all` then drops it once it has been packed.
///
/// No `delete_pallet_ref` here, and no direct `collect_garbage` call: every step is a CLI command a
/// user runs. If this passes, the anchor reader is a live defect, not a prerequisite of the verb.
#[test]
fn the_false_tampering_state_is_reachable_with_shipped_commands_only() {
    let warehouse = Warehouse::new_unenrolled("live");

    let legacy = warehouse.stack("app.txt", "v1\n", "legacy one");
    let b = warehouse.stack("app.txt", "v2\n", "legacy two");

    warehouse.run_ok(&["office", "enroll"]);

    let boundary = warehouse.scoped(|| {
        office_utils::read_trust_anchor().unwrap().expect("an anchor").boundary
    });
    assert_eq!(boundary, vec![b.clone()], "the boundary must be exactly [b]");

    // Pack, so the repack sweep is able to drop `b` (a repack only drops already-packed garbage —
    // `pack_utils.rs:1659-1660`).
    warehouse.run_ok(&["compact"]);

    // Soft undo moves `main` back off `b`, orphaning it. No second ref is needed: `main` itself
    // lands on `legacy`, keeping it alive.
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
        "LIVE: b present = {}; legacy present = {}; audit exit = {:?}\n{}",
        b_present, legacy_present, audit.status.code(), out.trim()
    );

    assert!(!b_present, "the boundary head must have been reclaimed by `compact --all`");
    assert!(legacy_present, "the legacy parcel must survive as `main`'s head");
    assert!(
        !audit.status.success(),
        "audit succeeded — the state is NOT reachable without a ref unlink after all"
    );
    assert!(out.contains("tampered"), "expected the tampering accusation: {}", out);

    // The second false claim, asserted rather than printed — review found this one printed-only in
    // the leg below, for the third time in this document's history.
    assert!(
        out.contains("was stacked after trust was established"),
        "expected the post-trust misdating of a pre-trust parcel: {}",
        out
    );
}

/// §4.1b: is the anchor-`boundary` false-tampering state CONSTRUCTIBLE?
///
/// The design claimed a legacy (pre-trust) parcel whose attesting boundary head has been collected
/// makes `audit` fail the whole warehouse with "may have been tampered with"
/// (`audit_utils.rs:354-375`). Review objected that the conditions fight each other: gc drops what is
/// unreachable from the refs, and the boundary walk is reachability over the *same* parent edges, so a
/// legacy parcel whose only attesting head was collected is normally collected with it.
///
/// The seam that separates them: the boundary is a **snapshot at enroll time**. Create a second ref at
/// an *ancestor* AFTER the snapshot, then drop the ref holding the boundary head. The ancestor stays
/// alive (the new ref holds it) while its only boundary attestation is collected.
#[test]
fn a_legacy_parcel_outlives_the_boundary_head_that_attested_it() {
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

    // Drop `main`. `b` becomes reachable from no ref; `legacy` stays alive through `keep`.
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
        "ANCHOR: gc deleted {}; b present = {}; legacy present = {}; audit exit = {:?}\n{}",
        stats.deleted, b_present, legacy_present, audit.status.code(), out.trim()
    );

    // The fixture itself: the attesting head is gone, the attested parcel is not.
    assert!(!b_present, "the boundary head must have been collected");
    assert!(legacy_present, "the legacy parcel must survive through `keep`");

    // THE CLAIM UNDER TEST. If this fails, the review's objection stands and §4.1b's hazard is not
    // constructible this way — which retires the last argument for changing any root set.
    assert!(
        !audit.status.success(),
        "audit SUCCEEDED over a legacy parcel with no surviving boundary attestation — the \
         false-tampering hazard is NOT constructible this way"
    );
    assert!(
        out.contains("tampered"),
        "audit failed for some other reason than the boundary attestation: {}",
        out
    );
    assert!(
        out.contains(&legacy),
        "the tampering error must name the legacy parcel {}: {}",
        legacy, out
    );

    // The SECOND false claim, asserted rather than printed. Review found this printed-only — the
    // third occurrence of that error in this work — so the "two false claims, not one" statement had
    // no falsifier: rewording `audit_utils.rs:362-366` would have left this leg green while the
    // claim quietly stopped being about anything.
    assert!(
        out.contains("was stacked after trust was established"),
        "the message must also misdate this pre-trust parcel as post-trust: {}",
        out
    );
}

/// FORK-82 — THE ACTING READER. `CherryPickState.source` is a durable parcel hash with no GC
/// root, and unlike the rendering members of this class its reader *acts* on the hash rather
/// than printing it.
///
/// The pin: `CherryPickState.source` (`cherry_pick_utils.rs:33`) is written to disk by
/// `write_state` (`:88-90`). It is not rooted — `gc_utils::collect_live_set` roots pallet heads,
/// bay-scoped parcels and `anchor.adopts` (`gc_utils.rs:136-171`), and a `grep` for "cherry" in
/// `gc_utils.rs`/`bay_utils.rs` returns nothing.
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

    // THE DEFECT, asserted rather than printed. Three separate claims, because the candidate
    // fixes invert different subsets of them and a single assertion would not say which landed:
    //
    //   * root the pick source      -> `stack` SUCCEEDS; all three below invert.
    //   * presence check + remedy   -> `stack` still fails; the first holds, the last two invert.
    //   * abort path only           -> `stack` still fails; only the "abort" leg inverts.
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
    assert!(
        !err.to_lowercase().contains("cherry-pick"),
        "the error now names the cherry-pick, so it is no longer unattributed — invert this leg. \
         stderr: {}",
        err
    );
    assert!(
        !err.to_lowercase().contains("abort"),
        "the error now advertises an abort path, so the user is no longer stranded with resolved \
         conflicts and no way out — invert this leg. stderr: {}",
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

    // THE WEDGE, asserted. Four separate claims; the control leg below is what makes them
    // attributable to the absent tag subject rather than to torn-ness.
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
    assert_eq!(
        ordinary.status.code(), Some(21),
        "the refusal must be STORE-WIDE, not heal's own exit code: an ordinary `stocktake` has \
         to be refused too. got {:?}, stderr: {}",
        ordinary.status.code(), ordinary_err
    );

    // No in-tool exit. If a retire/delete verb ever lands, this is the assertion to invert —
    // and the wedge stops being a wedge.
    assert!(
        !help.contains("delete") && !help.contains("retire") && !help.contains("remove"),
        "`tag` grew a way to retire a tag, so the wedge now has an in-tool exit — invert this \
         leg and revisit FORK-83's premise. help was:\n{}",
        help
    );

    // AND THE REMEDY IS CIRCULAR — the part the ticket does not state. The refusal that blocks
    // every command tells the operator to run the one command that just refused.
    assert!(
        ordinary_err.contains("forklift heal"),
        "expected the store-wide refusal to direct the operator to `forklift heal`; stderr: {}",
        ordinary_err
    );
}

/// CONTROL for the leg above: is the *tag* actually necessary to wedge the store?
///
/// The wedge leg reports TWO dangling references, and only one of them is the tag subject. The
/// other is byte-identical across runs whose every other hash differs, so it is a fixed-content
/// object rather than anything this fixture created. This leg removes the tag, the `undo` and the
/// collection entirely — a warehouse that has done nothing but `stack` — and applies only the
/// torn taint.
///
/// If this refuses, the wedge is not a tag-subject defect at all and FORK-83's framing is too
/// narrow; if it heals cleanly, the tag subject is load-bearing and the second hash is incidental
/// to it.
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
