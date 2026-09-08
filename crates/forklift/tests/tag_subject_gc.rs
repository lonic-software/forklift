//! FORK-79: `Tag.subject` is a bare parcel hash inside `@tags` (tag_utils.rs:51). Nothing roots
//! it — `gc_utils::collect_live_set` marks pallet heads, bay-scoped parcels and the trust pins
//! (`office_utils::collect_trust_pin_roots`: the anchor's `adopts`, its enrollment `boundary`,
//! and every key's revocation `distrust_boundary` — FORK-81). A tag subject is in none of those
//! sets, which is what this file is about. `undo` moves a pallet head *backwards* to
//! `pre_head` (journal_utils.rs:191), so tagging a parcel and then undoing the stack that
//! created it leaves the tag's subject reachable from no ref at all — and gc collects it.
//!
//! This needs no pallet-deletion verb, which is why it is a live defect: `tag show` used to name
//! an absent parcel with a success exit and no marker at all — a silent misread. The settled fix
//! is a presence *probe* at the reader (`crate::commands::tag::probe_subject`), not rooting the
//! subject (see `tag_utils.rs` and `gc_utils.rs` for why rooting was refuted) — and not a refusal
//! either: `show` renders and marks rather than refuses, because "collected here" and "never
//! fetched here" are the same local state, and a sparse or franchised clone legitimately holds
//! tags whose subject it never fetched. Only a genuinely indeterminate probe (an I/O error, never
//! a definite answer) is still an ordinary command error — see the third test below.
//!
//! Round 2 of review added a third classification: `Tag.subject` is an unvalidated free string
//! (`tag_utils.rs:328`, `read_string("subject")`), and `@tags` records sync in wholesale via
//! `adopt_meta_pallets`, so a foreign or older client can author one that is not even hash-shaped.
//! Before that fix, `probe_subject`'s presence check (`file_utils::does_object_exist`) reached
//! `get_path_for_object`, which refuses a non-hash string outright — turning one bad record into
//! an anonymous, whole-command failure for `tag list`/`tag show` alike. A malformed subject is
//! now classified explicitly (`file_utils::is_valid_object_hash`) and marks, exactly like an
//! ordinary absence, with its own accurate wording. **This case is not covered end-to-end here**:
//! no shipped `forklift` command can construct a malformed-subject `@tags` record locally (`tag
//! create`'s only two subject sources already verify hash shape and object presence before
//! returning — `pallet_utils::resolve_revision`/`get_pallet_head`), and hand-planting one on disk
//! would mean reproducing `LooseObject::store`'s compression from the test, not just writing
//! plain bytes the way `plant_taint` does for a taint record elsewhere in this suite — that is
//! exactly the "reach into internals" this was told not to do. See
//! `crate::commands::tag::tests` (`crates/forklift/src/commands/tag.rs`) for the unit-level
//! coverage of the classification and rendering logic instead.
//!
//! Round 3 found round 2's fix incomplete: `parse_tag` validates none of `Tag`'s fields (only
//! `tag create` validates the one it creates, `name`), so `name`/`subject`/`message`/`tagger` are
//! *all* remote-authored and arbitrary, and the render path did not honour that — a subject
//! sliced by byte range (`&s[..12]`) could panic on multi-byte UTF-8, and `name` renders even
//! further ahead of the marker than `message` does, so round 2's positional fix did not cover it.
//! The fix moved from field-by-field patching to a boundary: every tag-record string a human
//! render prints goes through `crate::commands::tag::render_safe` (neutralizes control
//! characters, including ANSI escapes) and, where truncated for display,
//! `crate::commands::tag::truncate_chars` (character-safe, never a byte range). A name that fails
//! `tag create`'s own naming rules is surfaced (`TagView::name_invalid`) rather than hidden or
//! rejected, the same way a malformed subject already was. `--json` is untouched by any of this —
//! `TagView`'s stored fields are never sanitized, only the copies `render_human` prints are; see
//! `list_row_fields_cannot_forge_hide_or_corrupt_the_absent_marker`'s own `--json` leg. The
//! multi-byte-subject panic and the name-forgery case are both unreachable through the shipped
//! CLI for the same reason the malformed-subject case is (see above) — pinned at the unit level
//! instead, in `crate::commands::tag::tests`.
//!
//! This file is trimmed from a shared spike (`boundary_gc.rs`, on another branch) down to the
//! tag leg and the harness it actually needs; the anchor-boundary legs are FORK-81's evidence
//! and stay behind on that branch.

use std::path::PathBuf;
use std::process::{Command, Output};

use forklift_core::globals::StorageRootScope;
use forklift_core::util::{file_utils, gc_utils};

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

    fn new_unenrolled(name: &str) -> Warehouse {
        let base =
            std::env::temp_dir().join(format!("forklift-tag-subject-gc-{}-{}", name, std::process::id()));
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

/// Assert the sweep actually ran: the unpinned, unreachable canary is gone. The tag leg below
/// calls this before touching the tag's subject at all — it is what stops "the subject
/// survived" (the fixed-state assertion) from being satisfiable by a collector that deleted
/// nothing.
fn assert_the_sweep_ran(warehouse: &Warehouse, canary: &str, deleted: usize) {
    let canary_present = warehouse.scoped(|| file_utils::does_object_exist(canary).unwrap());

    assert!(
        !canary_present,
        "the collector did not sweep: the unpinned, unreachable canary {} survived",
        canary
    );
    assert!(deleted > 0, "the collector reported deleting nothing");
}

/// THE SAME CLASS AS THE BOUNDARY-PIN HAZARD, REACHABLE WITH NO DELETION VERB AT ALL — a signed
/// tag's subject.
///
/// Builds the state `undo` produces (a tag whose subject a pallet head no longer reaches), runs
/// gc over it, and checks both the object store (the subject is gone) and every reader surface
/// FORK-79 touches: `tag show` (must still succeed — exit 0 — but render a warning naming the
/// hash), the same over `--json` (the envelope must carry `subject_absent: true`), and `tag
/// list` (must still succeed, and mark the row).
///
/// Assertions pin the hash and the structured `subject_absent` field — never prose adjectives
/// like "missing" — so wording can be tuned without reddening this test. The `show` legs assert
/// on the marker itself, not just the exit code: an exit-0 assertion alone would pass against a
/// completely unmarked render, which is exactly the silent-misread shape this ticket exists to
/// remove.
#[test]
fn a_signed_tag_subject_is_collected_after_undo_moves_the_head_back() {
    let warehouse = Warehouse::new("tag");

    warehouse.stack("app.txt", "v1\n", "first");

    // The canary: built and orphaned BEFORE the stack this test undoes, so `undo` (which targets
    // the newest journal entry) reverts the right thing. See `assert_the_sweep_ran`.
    warehouse.run_ok(&["palletize", "canary"]);
    let canary = warehouse.stack("canary.txt", "c1\n", "canary work");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.delete_pallet_ref("canary");

    let tagged = warehouse.stack("app.txt", "v2\n", "second");

    warehouse.run_ok(&["tag", "create", "v1.0", &tagged, "-m", "release"]);

    // Soft undo: the head moves back to `first`, so nothing reachable from a ref names `tagged`.
    warehouse.run_ok(&["undo"]);

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    let present = warehouse.scoped(|| file_utils::does_object_exist(&tagged).unwrap());

    println!(
        "TAG: gc deleted {} object(s); tagged parcel {} present = {}",
        stats.deleted, tagged, present
    );

    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);
    assert!(
        !present,
        "the tag's subject survived — the fixture is wrong (this must hold regardless of the fix)"
    );

    // `tag show` must still succeed — exit 0 — over a collected subject: refusing would
    // over-fire on a healthy sparse/franchised clone that never fetched this pallet at all (see
    // the module doc comment). It must still name the hash and mark it, not render it silently.
    let show = warehouse.run(&["tag", "show", "v1.0"]);
    let show_stdout = String::from_utf8_lossy(&show.stdout).to_string();
    let show_stderr = String::from_utf8_lossy(&show.stderr).to_string();

    println!(
        "TAG: `tag show v1.0` exit = {:?}; stdout = {}; stderr = {}",
        show.status.code(), show_stdout.trim(), show_stderr.trim()
    );

    assert!(
        show.status.success(),
        "`tag show` over a collected subject must still exit 0 (a marked render, not a \
        refusal); stdout: {} / stderr: {}",
        show_stdout, show_stderr
    );
    // The defining assertion: the marker itself, not just the exit code — an exit-0 assertion
    // alone would pass against a completely unmarked render (the silent misread this ticket
    // exists to remove).
    assert!(
        show_stdout.to_lowercase().contains("warning") && show_stdout.contains(&tagged),
        "the render must carry a warning naming the absent subject hash {} verbatim; stdout: {}",
        tagged, show_stdout
    );

    // The same render over `--json`: the envelope must carry `subject_absent: true` — this pins
    // the public machine contract, not just the human prose.
    let show_json = warehouse.run(&["--json", "tag", "show", "v1.0"]);
    let show_json_stdout = String::from_utf8_lossy(&show_json.stdout).to_string();
    let show_json_value: serde_json::Value =
        serde_json::from_str(&show_json_stdout).expect("tag show --json must parse");

    println!(
        "TAG: `--json tag show v1.0` exit = {:?}; stdout = {}",
        show_json.status.code(), show_json_stdout.trim()
    );

    assert!(show_json.status.success(), "the --json leg must also exit 0");
    assert_eq!(
        show_json_value["data"]["subject_absent"], serde_json::Value::Bool(true),
        "the --json envelope must carry subject_absent: true; stdout: {}",
        show_json_stdout
    );
    assert_eq!(
        show_json_value["data"]["subject"], serde_json::Value::String(tagged.clone()),
        "the --json envelope must also carry the absent subject hash {}; stdout: {}",
        tagged, show_json_stdout
    );

    // `tag list` must still succeed (exit 0) — a list is an enumeration, not a claim about one
    // referent — but must not render the dead hash indistinguishably from a live one.
    let list = warehouse.run(&["tag", "list"]);
    let list_stdout = String::from_utf8_lossy(&list.stdout).to_string();

    println!(
        "TAG: `tag list` exit = {:?}; stdout = {}",
        list.status.code(), list_stdout.trim()
    );

    assert!(
        list.status.success(),
        "`tag list` must stay exit 0 even when a subject is absent; stdout: {}",
        list_stdout
    );
    // The human row truncates the subject to 12 characters (like every other tag row); the
    // full hash is pinned separately below, over `--json`.
    assert!(
        list_stdout.contains(&tagged[..12]),
        "`tag list` must still show (a prefix of) the tag's subject hash {}; stdout: {}",
        tagged, list_stdout
    );
    assert!(
        list_stdout.to_lowercase().contains("not in this store"),
        "`tag list` must mark the row as having an absent subject; stdout: {}",
        list_stdout
    );

    let list_json = warehouse.run(&["--json", "tag", "list"]);
    let list_json_value: serde_json::Value =
        serde_json::from_slice(&list_json.stdout).expect("tag list --json must parse");

    println!("TAG: `--json tag list` = {}", list_json_value);

    assert!(list_json.status.success(), "`--json tag list` must also stay exit 0");
    let tags = list_json_value["data"]["tags"].as_array().expect("a tags array");
    let entry = tags.iter().find(|tag| tag["name"] == "v1.0").expect("the v1.0 entry");
    assert_eq!(
        entry["subject_absent"], serde_json::Value::Bool(true),
        "the --json list entry must carry subject_absent: true; entry: {}",
        entry
    );
}

/// Build a warehouse holding a tag `"v1.0"` whose subject alone occupies its fan-out
/// directory (its own `.sig` sidecar aside) — the precondition
/// `tag_list_and_show_fail_loudly_and_name_the_tag_when_the_probe_is_indeterminate` needs
/// before it can revoke that directory's permissions and mean only the subject by it.
///
/// `tag create` does not only touch the subject: it also stores the `@tags` parcel itself,
/// the tree chain leading to it (`.forklift`, `tracked`, `tags`), and the tag record blob —
/// each a distinct object, each landing in a fan-out directory keyed by ITS OWN hash, which by
/// pure hash coincidence (~1-in-256 per object) can be the same two-hex-character directory as
/// the subject. When that happens, revoking the directory collaterally blocks that other
/// object too, and since `tag_utils::read_tags` reads the parcel/tree chain/record blob with a
/// bare `?` before it has parsed any tag name at all, a collision there makes `tag
/// list`/`show` fail with the raw, anonymous loose-object-read error instead of
/// `probe_subject`'s tag-naming one — a fixture-caused false red. Confirmed directly (not
/// merely suspected): forcing this collision reproduces `Error while reading object from file
/// "…/<hash>": Permission denied (os error 13)` verbatim, with no "v1.0" in it anywhere; the
/// colliding object varies by run (observed once as the @tags parcel's own intermediate tree
/// object) — any object `read_tags` walks can be the one.
///
/// Retries with fresh content on collision, up to `MAX_ATTEMPTS` times: a tag name is
/// immutable, so the same warehouse cannot retry the same name, and each attempt therefore
/// rebuilds a fresh warehouse (`Warehouse::new` wipes its own temp directory). Checked right
/// after `tag create` returns — the point the precondition must hold — and before anything
/// touches permissions. Bounded and loud on exhaustion rather than looping forever or silently
/// proceeding with a directory that is not actually exclusive.
fn warehouse_with_an_exclusive_tag_subject(name: &str) -> (Warehouse, String) {
    const MAX_ATTEMPTS: usize = 500;

    for attempt in 0..MAX_ATTEMPTS {
        let warehouse = Warehouse::new(name);
        let head = warehouse.stack("app.txt", &format!("v1-attempt-{}\n", attempt), "first");
        warehouse.run_ok(&["tag", "create", "v1.0", &head, "-m", "release"]);

        let fan_out_dir = warehouse.root.join(".forklift").join("objects").join(&head[..2]);
        let subject_suffix = head[2..].to_string();
        let sig_suffix = format!("{}.sig", subject_suffix);

        let entries: Vec<String> = std::fs::read_dir(&fan_out_dir)
            .unwrap_or_else(|error| panic!("could not list {}: {}", fan_out_dir.display(), error))
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();

        let foreign: Vec<&String> = entries.iter()
            .filter(|entry| entry.as_str() != subject_suffix && entry.as_str() != sig_suffix)
            .collect();

        if foreign.is_empty() {
            return (warehouse, head);
        }

        println!(
            "PROBE-FAILURE FIXTURE: attempt {} collided — {} foreign object(s) share the \
            subject's fan-out directory {}: {:?}; rebuilding with different content",
            attempt, foreign.len(), fan_out_dir.display(), foreign
        );
    }

    panic!(
        "could not build a tag subject with an exclusive fan-out directory in {} attempts; \
        the fixture's collision precondition should hold within a handful of tries at roughly \
        1-in-256 odds per @tags object — this many consecutive collisions points at something \
        other than chance",
        MAX_ATTEMPTS
    );
}

/// `tag list`/`tag show`'s presence probe (`file_utils::does_object_exist`) can fail for a
/// reason other than "the subject is not here": the loose-object read itself can error. That is
/// a genuinely different answer from a definite absence, and must not be folded into
/// `subject_absent: true` — this ticket exists to stop `tag` from asserting things it has not
/// established, and mislabeling an unknown as absent is the same defect wearing a different
/// hash. Review round 2 added a second requirement over the same failure: the command's error
/// must name the offending tag — round 1's fix propagated `does_object_exist`'s raw, anonymous
/// error, so a user hitting this had no way to find which record was responsible.
///
/// This test constructs the probe failure as a plain I/O error, not a durability taint: revoke
/// read/execute permission on the loose object's containing fan-out directory, so
/// `does_object_exist` cannot even determine presence. `does_object_exist` also consults a
/// durability-taint gate (`taint_utils::gate_check`), a second real source of the same kind of
/// failure in that function — but it could not be used here, because that gate is
/// process-local, in-memory state set only by a write *this same process* performed and failed
/// to sync, so it is never visible to a fresh CLI subprocess; a taint left standing on disk by
/// an earlier process is instead intercepted by `main.rs`'s own entry-heal chokepoint before any
/// command's body (including `list`'s/`show`'s) ever runs, so neither would ever reach its own
/// probe in that case at all. The I/O failure this test builds instead exercises the identical
/// contract the fix adds — propagate an indeterminate probe, naming the tag, rather than
/// mislabel it — through the same function both commands call (`probe_subject`).
///
/// PRECONDITION — the subject must be the *only* object in its fan-out directory (its own
/// `.sig` sidecar aside) before permissions are revoked. `tag create` writes more than the
/// subject: the `@tags` parcel, the tree chain leading to it, and the tag record blob are each
/// distinct objects that land in fan-out directories keyed by their OWN hash, and by pure
/// hash coincidence (~1-in-256 per object) one of them can land in the SAME directory as the
/// subject. `tag_utils::read_tags` reads that parcel/tree chain/record blob with a bare `?`
/// before it has parsed any tag name at all, so when the collateral object is what the
/// revoked directory blocks, `read_tags` fails first with the raw, anonymous loose-object-read
/// error — never reaching `probe_subject`, so the failure never names "v1.0" and this test
/// goes red for a reason that has nothing to do with the contract under test. This is not
/// theoretical: two 2026-08-01 CI runs hit exactly this anonymous failure on two different
/// hash prefixes, and forcing the collision deliberately reproduces it verbatim (see
/// `warehouse_with_an_exclusive_tag_subject`, which this test uses to build a warehouse where
/// the precondition provably holds, retrying with fresh content — bounded, and loud on
/// exhaustion — until the subject's fan-out directory is exclusive to it).
#[cfg(unix)]
#[test]
fn tag_list_and_show_fail_loudly_and_name_the_tag_when_the_probe_is_indeterminate() {
    use std::os::unix::fs::PermissionsExt;

    let (warehouse, head) = warehouse_with_an_exclusive_tag_subject("probe-failure");

    // The loose object's one-level fan-out directory (`file_utils::get_path_for_object`: the
    // first 2 hex chars of the hash name it). Revoking access here makes `std::fs::exists`
    // return `Err` instead of `Ok(false)` — the store cannot say whether the object is there.
    // `warehouse_with_an_exclusive_tag_subject` has already established that the subject (and
    // its `.sig` sidecar) are the only things this revoke can collaterally touch.
    let fan_out_dir = warehouse.root.join(".forklift").join("objects").join(&head[..2]);

    // Captured while the directory is still readable, purely as a diagnostic: if the
    // tag-naming assertions below fail anyway, for a reason other than the collision this
    // fixture now guards against, this dump (plus the subject hash) makes a future occurrence
    // self-diagnosing instead of another anonymous report.
    let fan_out_contents_before_revoke: Vec<String> = std::fs::read_dir(&fan_out_dir)
        .unwrap_or_else(|error| panic!("could not list {}: {}", fan_out_dir.display(), error))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();

    std::fs::set_permissions(&fan_out_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let list = warehouse.run(&["tag", "list"]);
    let list_json = warehouse.run(&["--json", "tag", "list"]);
    let show = warehouse.run(&["tag", "show", "v1.0"]);
    let show_json = warehouse.run(&["--json", "tag", "show", "v1.0"]);

    // Restore access before any assertion can panic and leave an unreadable directory behind
    // for the OS temp-dir cleanup (or a future run) to trip over.
    std::fs::set_permissions(&fan_out_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let stdout = String::from_utf8_lossy(&list.stdout).to_string();
    let stderr = String::from_utf8_lossy(&list.stderr).to_string();
    let json_stdout = String::from_utf8_lossy(&list_json.stdout).to_string();
    let show_stdout = String::from_utf8_lossy(&show.stdout).to_string();
    let show_stderr = String::from_utf8_lossy(&show.stderr).to_string();
    let show_json_stdout = String::from_utf8_lossy(&show_json.stdout).to_string();

    println!(
        "TAG: `tag list` over an unprobeable subject: exit = {:?}; stdout = {}; stderr = {}",
        list.status.code(), stdout.trim(), stderr.trim()
    );
    println!(
        "TAG: `--json tag list` over an unprobeable subject: exit = {:?}; stdout = {}",
        list_json.status.code(), json_stdout.trim()
    );
    println!(
        "TAG: `tag show v1.0` over an unprobeable subject: exit = {:?}; stdout = {}; stderr = {}",
        show.status.code(), show_stdout.trim(), show_stderr.trim()
    );
    println!(
        "TAG: `--json tag show v1.0` over an unprobeable subject: exit = {:?}; stdout = {}",
        show_json.status.code(), show_json_stdout.trim()
    );

    assert!(
        !list.status.success(),
        "`tag list` must fail loudly when it cannot even determine presence; stdout: {} / \
        stderr: {}",
        stdout, stderr
    );
    assert!(
        !list_json.status.success(),
        "the --json `list` leg must also fail loudly; stdout: {}",
        json_stdout
    );
    assert!(
        !show.status.success(),
        "`tag show` must fail loudly when it cannot even determine presence; stdout: {} / \
        stderr: {}",
        show_stdout, show_stderr
    );
    assert!(
        !show_json.status.success(),
        "the --json `show` leg must also fail loudly; stdout: {}",
        show_json_stdout
    );

    // Nothing here claims the subject is absent — an unknown must never be printed as though it
    // were a definite negative.
    assert!(
        !stdout.contains("subject_absent") && !json_stdout.contains("subject_absent"),
        "an indeterminate probe must never be reported as subject_absent; stdout: {} / json: {}",
        stdout, json_stdout
    );
    assert!(
        !stdout.to_lowercase().contains("not in this store"),
        "an indeterminate probe must never be worded as a definite absence; stdout: {}",
        stdout
    );

    // THE NEW ASSERTION: the failure names the tag, so a user can find the offending record —
    // round 1's error was anonymous (`does_object_exist`'s raw message alone). If this ever
    // fails despite `warehouse_with_an_exclusive_tag_subject`'s precondition, the failure
    // message below dumps the subject hash and the fan-out directory's pre-revoke contents so
    // the occurrence is self-diagnosing rather than another anonymous report.
    assert!(
        stderr.contains("v1.0"),
        "`tag list`'s failure must name the affected tag \"v1.0\"; stderr: {} (subject {}; \
        fan-out dir {} held {:?} before permissions were revoked)",
        stderr, head, fan_out_dir.display(), fan_out_contents_before_revoke
    );
    assert!(
        show_stderr.contains("v1.0"),
        "`tag show`'s failure must name the affected tag \"v1.0\"; stderr: {} (subject {}; \
        fan-out dir {} held {:?} before permissions were revoked)",
        show_stderr, head, fan_out_dir.display(), fan_out_contents_before_revoke
    );
}

/// Round 2 found: `TagList::render_human`'s absent-subject marker trailed the row, after the
/// tag's own message — remote-authored, unvalidated text a franchise/lower can bring in from
/// anyone. A message ending in text that imitates the marker could forge marking on a perfectly
/// healthy row, and an ANSI escape at the start of a message could restyle or blank a real
/// marker that preceded it. The marker now renders ahead of the tagger and message, where
/// neither can reach it.
///
/// Round 3 found the fix incomplete: **every** `@tags` string is remote-authored (`parse_tag`
/// validates nothing; only `tag create` validates its own `name`, never a synced-in one), and
/// `name` renders even further ahead than the marker — first in the row. A `name` containing
/// space/paren characters could, in principle, spell a fake marker and a fake " by " boundary
/// ahead of the real one. It cannot reach that state through `tag create` here (`validate_tag_
/// name`'s charset excludes space and `(`/`)` outright, so this test cannot construct one — see
/// `crate::commands::tag::tests::tag_view_of_sets_name_invalid_only_for_a_name_tag_create_would_
/// never_produce`, which pins the same defense — a `[invalid name] ` prefix, printed by code
/// ahead of anything `name` supplies — at the unit level instead), but two things ARE reachable
/// here and are what this test now covers: an ANSI/control-character forgery through `message`
/// (unvalidated at every layer, so directly constructible via `-m`), and that `--json` keeps
/// message/name raw — sanitizing is a terminal-rendering concern, not a wire-format one.
#[test]
fn list_row_fields_cannot_forge_hide_or_corrupt_the_absent_marker() {
    let warehouse = Warehouse::new("marker-spoof");
    const MARKER: &str = "(subject not in this store)";
    const ANSI_MESSAGE: &str = "\x1b[31mHIDDEN\x1b[0m release";

    // A healthy tag whose message spells out the marker text verbatim — an attempt to forge
    // marking on a live row purely through message content.
    let live_head = warehouse.stack("app.txt", "v1\n", "first");
    let spoofing_message = format!("{} release", MARKER);
    warehouse.run_ok(&["tag", "create", "v-live", &live_head, "-m", &spoofing_message]);

    // A healthy tag whose message carries a raw ANSI/SGR escape sequence — `message` has no
    // validation at all (unlike `name`), so this is reachable through the shipped CLI directly,
    // with a literal ESC byte in the `-m` argument.
    let ansi_head = warehouse.stack("app.txt", "v1.5\n", "ansi");
    warehouse.run_ok(&["tag", "create", "v-ansi", &ansi_head, "-m", ANSI_MESSAGE]);

    // A genuinely absent-subject tag, built the same way
    // `a_signed_tag_subject_is_collected_after_undo_moves_the_head_back` does.
    warehouse.run_ok(&["palletize", "canary"]);
    let canary = warehouse.stack("canary.txt", "c1\n", "canary work");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.delete_pallet_ref("canary");

    let tagged = warehouse.stack("app.txt", "v2\n", "second");
    warehouse.run_ok(&["tag", "create", "v-absent", &tagged, "-m", "release"]);
    warehouse.run_ok(&["undo"]);

    let stats = warehouse.scoped(|| gc_utils::collect_garbage(0).expect("gc runs"));
    assert_the_sweep_ran(&warehouse, &canary, stats.deleted);

    let list = warehouse.run(&["tag", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout).to_string();

    println!("TAG: `tag list` with a message-embedded marker lookalike and ANSI: stdout =\n{}", stdout);

    assert!(list.status.success(), "`tag list` must still succeed; stdout: {}", stdout);

    let live_line = stdout.lines().find(|line| line.starts_with("v-live"))
        .unwrap_or_else(|| panic!("no \"v-live\" row in: {}", stdout));
    let absent_line = stdout.lines().find(|line| line.starts_with("v-absent"))
        .unwrap_or_else(|| panic!("no \"v-absent\" row in: {}", stdout));
    let ansi_line = stdout.lines().find(|line| line.starts_with("v-ansi"))
        .unwrap_or_else(|| panic!("no \"v-ansi\" row in: {}", stdout));

    let by_pos_live = live_line.find(" by ").expect("every row has \" by \"");
    let by_pos_absent = absent_line.find(" by ").expect("every row has \" by \"");

    // THE GENUINE MARKER: must render before " by " on the truly absent row.
    let absent_marker_pos = absent_line.find(MARKER)
        .unwrap_or_else(|| panic!("the genuinely absent row must carry the marker: {}", absent_line));
    assert!(
        absent_marker_pos < by_pos_absent,
        "the genuine marker must render before \" by \" (ahead of tagger/message), not after; \
        row: {}",
        absent_line
    );

    // THE SPOOF ATTEMPT: the message-embedded lookalike text still literally appears in the
    // line (the message renders verbatim), but only ever after " by " — inside the message
    // region, never in the marker's own position — so it can never be confused with a genuine
    // marker by a reader (or a script) checking position rather than mere substring presence.
    let live_marker_pos = live_line.find(MARKER)
        .unwrap_or_else(|| panic!("the spoofing message must still appear verbatim: {}", live_line));
    assert!(
        live_marker_pos > by_pos_live,
        "a message-embedded marker lookalike must never appear before \" by \" — that would \
        make a healthy row indistinguishable from a genuinely marked one; row: {}",
        live_line
    );

    // THE ANSI ESCAPE: the raw ESC byte must never survive to a printed row — the whole point of
    // `render_safe`. Checked over the full `stdout`, not just `ansi_line`, so a leaked escape
    // that (mis)styles a *different* row still fails this.
    assert!(
        !stdout.contains('\x1b'),
        "a raw ESC byte must never reach rendered output; stdout: {}",
        stdout
    );
    assert!(
        ansi_line.contains("HIDDEN") && ansi_line.contains("[31m") && ansi_line.contains("[0m"),
        "the message's non-control text must still render (neutralized, not dropped); row: {}",
        ansi_line
    );

    // `--json` KEEPS THE RAW VALUES: sanitizing is a terminal-rendering concern, not a
    // wire-format one — a `--json` consumer needs the real bytes, ESC included.
    let list_json = warehouse.run(&["--json", "tag", "list"]);
    let list_json_value: serde_json::Value =
        serde_json::from_slice(&list_json.stdout).expect("tag list --json must parse");
    let tags = list_json_value["data"]["tags"].as_array().expect("a tags array");
    let ansi_entry = tags.iter().find(|tag| tag["name"] == "v-ansi")
        .unwrap_or_else(|| panic!("no v-ansi entry in: {}", list_json_value));

    assert_eq!(
        ansi_entry["message"], serde_json::Value::String(ANSI_MESSAGE.to_string()),
        "--json must carry the message's raw bytes, ESC included, unlike the human render; \
        entry: {}",
        ansi_entry
    );
}
