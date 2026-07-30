//! FORK-79: `Tag.subject` is a bare parcel hash inside `@tags` (tag_utils.rs:51). Nothing roots
//! it — `gc_utils::collect_live_set` marks only pallet heads, bay-scoped parcels and the
//! anchor's `adopts` pin (gc_utils.rs:136-171). `undo` moves a pallet head *backwards* to
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
#[cfg(unix)]
#[test]
fn tag_list_and_show_fail_loudly_and_name_the_tag_when_the_probe_is_indeterminate() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = Warehouse::new("probe-failure");

    let head = warehouse.stack("app.txt", "v1\n", "first");
    warehouse.run_ok(&["tag", "create", "v1.0", &head, "-m", "release"]);

    // The loose object's one-level fan-out directory (`file_utils::get_path_for_object`: the
    // first 2 hex chars of the hash name it). Revoking access here makes `std::fs::exists`
    // return `Err` instead of `Ok(false)` — the store cannot say whether the object is there.
    let fan_out_dir = warehouse.root.join(".forklift").join("objects").join(&head[..2]);
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
    // round 1's error was anonymous (`does_object_exist`'s raw message alone).
    assert!(
        stderr.contains("v1.0"),
        "`tag list`'s failure must name the affected tag \"v1.0\"; stderr: {}",
        stderr
    );
    assert!(
        show_stderr.contains("v1.0"),
        "`tag show`'s failure must name the affected tag \"v1.0\"; stderr: {}",
        show_stderr
    );
}

/// Finding 2 (review round 2): `TagList::render_human`'s absent-subject marker used to trail the
/// row, after the tag's own message — remote-authored, unvalidated text a franchise/lower can
/// bring in from anyone. A message ending in text that imitates the marker could forge marking
/// on a perfectly healthy row (`forklift tag create v9 <rev> -m "release (subject not in this
/// store)"`), and an ANSI escape at the start of a message could restyle or blank a real marker
/// that preceded it. The marker now renders ahead of the tagger and message, where neither can
/// reach it.
///
/// Builds one healthy tag (subject present) whose message spells out the exact marker text, and
/// one genuinely marked tag (subject absent, via the same undo-then-gc fixture the first test in
/// this file uses), then asserts the marker-lookalike in the healthy row's message can only ever
/// appear *after* " by " (inside the message region), while the genuine marker on the absent row
/// always appears *before* it — the position, not the text, is what a reader must trust.
#[test]
fn list_marker_cannot_be_forged_or_hidden_by_message_content() {
    let warehouse = Warehouse::new("marker-spoof");
    const MARKER: &str = "(subject not in this store)";

    // A healthy tag whose message spells out the marker text verbatim — an attempt to forge
    // marking on a live row purely through message content.
    let live_head = warehouse.stack("app.txt", "v1\n", "first");
    let spoofing_message = format!("{} release", MARKER);
    warehouse.run_ok(&["tag", "create", "v-live", &live_head, "-m", &spoofing_message]);

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

    println!("TAG: `tag list` with a message-embedded marker lookalike: stdout =\n{}", stdout);

    assert!(list.status.success(), "`tag list` must still succeed; stdout: {}", stdout);

    let live_line = stdout.lines().find(|line| line.starts_with("v-live"))
        .unwrap_or_else(|| panic!("no \"v-live\" row in: {}", stdout));
    let absent_line = stdout.lines().find(|line| line.starts_with("v-absent"))
        .unwrap_or_else(|| panic!("no \"v-absent\" row in: {}", stdout));

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
}
