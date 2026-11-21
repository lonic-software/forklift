//! A served warehouse whose tombstone list or served mark is lost or damaged. Whatever frees
//! history (gc, a full compaction) refuses it as `tombstones_damaged` (exit 55), naming
//! `forklift-server tombstone repair`; `heal` still finishes, saying so. The repair keeps what the
//! old list still says, gives a lost deleted pallet's head its name back only where the bays' undo
//! journals settle it, and keeps every other head the list may have held with no name, through gc
//! and compaction both, until `tombstone drop --head` releases it.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::common;
use common::forklift_command;

const TOMBSTONES_DAMAGED_EXIT: i32 = 55;

struct Area {
    base: PathBuf,
    home: PathBuf,
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = forklift_core::util::file_utils::remove_dir_all(&self.base);
    }
}

impl Area {
    fn new(name: &str) -> Area {
        let base = std::env::temp_dir().join(format!("forklift-tombstone-repair-{}-{}", name, std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let base = match base.to_str().and_then(|shown| shown.strip_prefix(r"\\?\")) {
            Some(plain) => PathBuf::from(plain),
            None => base,
        };
        let home = base.join("home");

        Area { base, home }
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run_ok(&self, dir: &Path, args: &[&str]) -> Output {
        let output = self.run(dir, args);
        assert!(output.status.success(), "{:?} in {:?} failed: {}{}", args, dir, text(&output.stdout), text(&output.stderr));
        output
    }

    /// `forklift-server` with `args`, `--root` the warehouse added.
    fn server(&self, root: &Path, args: &[&str]) -> Output {
        common::test_command(common::server_binary())
            .args(args)
            .args(["--root", root.to_str().unwrap()])
            .output()
            .unwrap()
    }

    fn server_ok(&self, root: &Path, args: &[&str]) -> String {
        let output = self.server(root, args);
        assert!(output.status.success(), "{:?} failed: {}{}", args, text(&output.stdout), text(&output.stderr));
        text(&output.stdout)
    }

    /// A prepared warehouse with one parcel on `main`, never served.
    fn warehouse(&self) -> PathBuf {
        let root = self.base.join("wh");
        std::fs::create_dir_all(&root).unwrap();
        self.run_ok(&root, &["prepare"]);
        self.run_ok(&root, &["config", "set", "operator.identifier", "tombstone-repair@forklift"]);
        self.stack(&root, "a.txt", "one");
        root
    }

    /// [`Area::warehouse`], marked served as a server marks it.
    fn served(&self) -> PathBuf {
        let root = self.warehouse();
        mark_served(&root);
        root
    }

    fn stack(&self, dir: &Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), format!("{}\n", message)).unwrap();
        self.run_ok(dir, &["load", "."]);
        self.run_ok(dir, &["stack", "-m", message]);
    }

    /// Make pallet `name` with one parcel of its own and delete it, the way a bay does (its undo
    /// journal records the delete); returns the head it was deleted at.
    fn deleted(&self, root: &Path, name: &str) -> String {
        self.run_ok(root, &["pallet", "create", name]);
        self.stack(root, &format!("{}.txt", name), &format!("{} work", name));
        let head = head(root, name);
        self.run_ok(root, &["shift", "main"]);
        self.run_ok(root, &["pallet", "delete", name, "--force"]);
        head
    }

    /// Run gc past any grace and a full compaction; both must succeed.
    fn collect_everything(&self, root: &Path) {
        self.server_ok(root, &["gc", "--grace-hours", "0"]);
        self.run_ok(root, &["store", "compact", "--all"]);
    }
}

/// A way to damage a served root's tombstones, and its name.
type Damage = (&'static str, fn(&Path));

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn head(root: &Path, pallet: &str) -> String {
    std::fs::read_to_string(root.join(".forklift").join("pallets").join(pallet)).unwrap().trim_end().to_string()
}

fn mark_served(root: &Path) {
    let _scope = forklift_core::globals::StorageRootScope::enter(root);
    assert!(forklift_core::util::tombstone_utils::mark_served(true).unwrap());
}

fn list_path(root: &Path) -> PathBuf {
    root.join(".forklift").join("tombstones")
}

/// Forget every undo journal, as when a delete's entry has fallen off the end.
fn forget_journals(root: &Path) {
    forklift_core::util::file_utils::remove_file(root.join(".forklift").join("journal.json")).unwrap();
}

/// Whether the store still holds `hash`, loose or packed.
fn holds(area: &Area, root: &Path, hash: &str) -> bool {
    area.run(root, &["store", "peek", hash]).status.success()
}

fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{}: {}{}", e, text(&output.stdout), text(&output.stderr)))
}

/// Revert: read an absent list in a served root as empty → gc and compaction run → red.
#[test]
fn a_served_root_whose_list_is_lost_refuses_whatever_frees_history() {
    let area = Area::new("refuses");
    let root = area.served();
    area.deleted(&root, "side");
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();

    let gc = area.server(&root, &["gc", "--grace-hours", "0"]);
    assert!(!gc.status.success(), "{}", text(&gc.stdout));
    assert!(text(&gc.stderr).contains("forklift-server tombstone repair --root"), "{}", text(&gc.stderr));

    let compact = area.run(&root, &["--json", "store", "compact", "--all"]);
    assert_eq!(compact.status.code(), Some(TOMBSTONES_DAMAGED_EXIT), "{}{}", text(&compact.stdout), text(&compact.stderr));
    let error = json_of(&compact)["error"].clone();
    assert_eq!(error["code"], "tombstones_damaged", "{}", error);
    assert_eq!(
        error["next_step"].as_str().unwrap(),
        format!("forklift-server tombstone repair --root {}", forklift_core::util::franchise_utils::quote(root.to_str().unwrap())),
        "the warehouse is named whole, wherever the remedy is run from"
    );
}

/// Revert: skip the lost-tip step → nothing keeps the deleted head and gc frees it → red.
/// Revert: let `tombstone drop --head` drop by name only → the kept head is refused as no
/// pallet's → red.
#[test]
fn a_repair_keeps_a_deleted_head_no_journal_names_until_it_is_dropped() {
    let area = Area::new("kept");
    let root = area.served();
    let side = area.deleted(&root, "side");
    forget_journals(&root);
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();

    let report = area.server_ok(&root, &["tombstone", "repair"]);
    assert!(report.contains("Kept 1 head with no name"), "{}", report);
    let listed = area.server_ok(&root, &["tombstone", "list"]);
    assert!(listed.contains(&side) && listed.contains("(kept, no name)"), "{}", listed);

    area.collect_everything(&root);
    assert!(holds(&area, &root, &side), "gc and a full compaction keep the kept head");

    area.server_ok(&root, &["tombstone", "drop", "--head", &side]);
    area.collect_everything(&root);
    assert!(!holds(&area, &root, &side), "once dropped, nothing holds it");
}

/// Revert: enumerate loose objects only → the packed tip is not found, so the full compaction
/// drops it → red.
#[test]
fn a_repair_keeps_a_head_held_only_in_a_pack() {
    let area = Area::new("packed");
    let root = area.served();
    let side = area.deleted(&root, "side");
    area.run_ok(&root, &["store", "compact"]);
    forget_journals(&root);
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();
    assert!(!root.join(".forklift").join("objects").join(&side[..2]).join(&side[2..]).exists(), "the tip is packed only");

    area.server_ok(&root, &["tombstone", "repair"]);
    area.collect_everything(&root);

    assert!(holds(&area, &root, &side));
}

/// A lost tip whose only copy is loose in a fan-out folder the repair cannot list: the repair
/// refuses, naming the folder, rather than finishing without it; once the folder lists again it
/// keeps the tip. Skipped as root, which lists any folder.
///
/// Three listings guard it: the store status the pack check reads first lists the loose objects
/// and the signature sidecars, and then the lost-tip enumeration lists the folder again. Revert:
/// let all three read an unlistable folder as empty → the repair finishes keeping nothing → red.
/// Any one of them alone keeps it green.
#[cfg(unix)]
#[test]
fn a_repair_refuses_a_fan_out_folder_it_cannot_list() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let area = Area::new("unlistable");
    let root = area.served();
    let objects = root.join(".forklift").join("objects");

    // The tip's folder must hold nothing else: a reachable object there would be read by the
    // walk before the listing, and refuse for a different reason.
    let (side, folder) = (0..20).find_map(|attempt| {
        let side = area.deleted(&root, &format!("side{}", attempt));
        let folder = objects.join(&side[..2]);
        let alone = std::fs::read_dir(&folder).unwrap()
            .all(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(&side[2..]));
        alone.then_some((side, folder))
    }).expect("a deleted tip alone in its fan-out folder");
    forget_journals(&root);
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).unwrap();
    let refused = area.server(&root, &["tombstone", "repair"]);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!refused.status.success(), "{}", text(&refused.stdout));
    let stderr = text(&refused.stderr);
    assert!(stderr.contains(&format!("objects/{}", &side[..2])), "{}", stderr);

    area.server_ok(&root, &["tombstone", "repair"]);
    let listed = area.server_ok(&root, &["tombstone", "list"]);
    assert!(listed.contains(&side) && listed.contains("(kept, no name)"), "{}", listed);
}

/// Revert: adopt a journal's name without checking every head the journals name for it → H1 is
/// listed as "side", though the lost list held side at H2 → red. Revert: take every deleted name
/// as a candidate for every tip → no tip has one candidate, so "gone" is not named → red.
#[test]
fn a_repair_names_a_deletion_only_where_the_journals_settle_it() {
    use forklift_core::util::pallet_utils::{self, PalletNamespace, RefPreconditions};
    let area = Area::new("named");
    let root = area.warehouse();

    // Before the first serve, "side" is deleted at H1 and made again at H2, which is not H1's
    // descendant; once served, it is deleted at H2 where no journal saw it (as over the protocol).
    let h1 = area.deleted(&root, "side");
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.stack(&root, "other.txt", "other");
    let h2 = head(&root, "side");
    area.run_ok(&root, &["shift", "main"]);
    mark_served(&root);
    let gone = area.deleted(&root, "gone");
    {
        let _scope = forklift_core::globals::StorageRootScope::enter(&root);
        pallet_utils::remove_pallet_head_in(PalletNamespace::User, "side", &h2, &RefPreconditions::NONE, None).unwrap();
    }
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();

    let report = area.server_ok(&root, &["tombstone", "repair"]);

    let listed = area.server_ok(&root, &["tombstone", "list"]);
    let line_of = |head: &str| listed.lines().find(|line| line.contains(head)).unwrap_or_else(|| panic!("{} is not kept: {}\n{}", head, listed, report)).to_string();
    assert!(line_of(&gone).contains("gone"), "{}", listed);
    assert!(line_of(&h1).contains("(kept, no name)"), "the journals do not settle H1: {}", listed);
    assert!(line_of(&h2).contains("(kept, no name)"), "no journal names H2 deleted: {}", listed);
    assert!(!listed.lines().any(|line| line.contains("side")), "nothing is named side: {}", listed);
}

/// Revert: name only lost tips → "gone" is deleted at a head that "side" was made from and moved
/// on from, so that head is a parent, not a tip, and "gone" loses its name → red.
#[test]
fn a_repair_names_a_deleted_head_that_unheld_history_descends_from() {
    let area = Area::new("descendant");
    let root = area.served();
    area.run_ok(&root, &["pallet", "create", "gone"]);
    area.stack(&root, "gone.txt", "gone work");
    let gone = head(&root, "gone");
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.stack(&root, "side.txt", "side work");
    let side = head(&root, "side");
    area.run_ok(&root, &["shift", "main"]);
    area.run_ok(&root, &["pallet", "delete", "side", "--force"]);
    area.run_ok(&root, &["pallet", "delete", "gone", "--force"]);
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();

    let report = area.server_ok(&root, &["tombstone", "repair"]);

    let listed = area.server_ok(&root, &["tombstone", "list"]);
    let line_of = |head: &str| listed.lines().find(|line| line.contains(head)).unwrap_or_else(|| panic!("{} is not kept: {}\n{}", head, listed, report)).to_string();
    assert!(line_of(&gone).contains("gone"), "{}", listed);
    assert!(line_of(&side).contains("side"), "{}", listed);
}

/// Revert: name only heads no durable holder reaches → "gone" was deleted where "main" stands, so
/// its name is not given back, and once main moves on nothing keeps that head → red.
#[test]
fn a_repair_names_a_deletion_at_a_head_another_pallet_still_holds() {
    let area = Area::new("held");
    let root = area.served();
    let main = head(&root, "main");
    area.run_ok(&root, &["pallet", "create", "gone"]);
    area.run_ok(&root, &["shift", "main"]);
    area.run_ok(&root, &["pallet", "delete", "gone", "--force"]);
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();

    area.server_ok(&root, &["tombstone", "repair"]);

    let listed = area.server_ok(&root, &["tombstone", "list"]);
    assert!(listed.lines().any(|line| line.contains(&main) && line.contains("gone")), "{}", listed);
}

/// Revert: leave a head kept with no name once a journal names it → the list holds it twice, and
/// after the name is restored the nameless entry still pins it → red.
#[test]
fn a_repair_that_names_a_head_the_damaged_list_kept_holds_it_once() {
    use forklift_core::util::record_utils;
    let area = Area::new("kept-named");
    let root = area.served();
    let gone = area.deleted(&root, "gone");
    let damaged = record_utils::to_bytes("tombstones", 3, [gone.as_str(), "not an entry"]);
    std::fs::write(list_path(&root), &damaged).unwrap();

    area.server_ok(&root, &["tombstone", "repair"]);

    let listed = area.server_ok(&root, &["tombstone", "list"]);
    let lines: Vec<&str> = listed.lines().filter(|line| line.contains(&gone)).collect();
    assert_eq!(lines.len(), 1, "{}", listed);
    assert!(lines[0].contains("gone"), "{}", listed);
}

/// Revert: run the whole repair when only the mark was lost → the journal settles "gone", whose
/// tombstone the operator dropped, so it is named again → red.
#[test]
fn a_repair_over_a_lost_mark_beside_a_whole_list_writes_only_the_mark() {
    let area = Area::new("mark-only");
    let root = area.served();
    let gone = area.deleted(&root, "gone");
    area.server_ok(&root, &["tombstone", "drop", "gone"]);
    let side = area.deleted(&root, "side");
    let before = std::fs::read(list_path(&root)).unwrap();
    forklift_core::util::file_utils::remove_file(root.join(".forklift").join("served")).unwrap();

    let report = area.server_ok(&root, &["tombstone", "repair"]);

    assert_eq!(std::fs::read(list_path(&root)).unwrap(), before, "a whole list is left as it was");
    // Revert: report a mark-only repair as a salvage → "Kept 0 entries of the old list" over a
    // list that still holds "side" → red.
    assert!(report.contains("The tombstone list reads whole, so it was left as it was."), "{}", report);
    assert!(!report.contains("Kept 0 entries") && !report.contains("no head was kept"), "nothing was salvaged: {}", report);
    assert!(root.join(".forklift").join("served").exists());
    let listed = area.server_ok(&root, &["tombstone", "list"]);
    assert!(listed.contains(&side) && !listed.contains(&gone), "{}", listed);

    // A mark of this version with no digest, as a build from before the digest wrote it, does not
    // read either. Revert: let the repair read the mark undigested → "nothing to repair" over a
    // mark every read refuses → red.
    std::fs::write(root.join(".forklift").join("served"), "/forklift-served 2\n").unwrap();
    let report = area.server_ok(&root, &["tombstone", "repair"]);
    area.server_ok(&root, &["tombstone", "list"]);

    // The damaged mark is set aside, and the whole list beside it is reported as left alone.
    // Revert: report the set-aside mark's repair as a salvage → "Kept 0 entries" → red.
    assert_eq!(std::fs::read(list_path(&root)).unwrap(), before, "a whole list is left as it was");
    assert!(report.contains("Moved the damaged") && report.contains("served.damaged-"), "{}", report);
    assert!(report.contains("The tombstone list reads whole, so it was left as it was."), "{}", report);
    assert!(report.contains("Wrote the served mark."), "{}", report);
    assert!(!report.contains("Kept 0 entries"), "nothing was salvaged: {}", report);
}

/// Revert: write only the kept heads → the entries that still read are lost → red.
#[test]
fn a_repair_keeps_what_a_damaged_list_still_says_and_sets_the_original_aside() {
    use forklift_core::util::record_utils;
    let area = Area::new("salvage");
    let root = area.served();
    let side = area.deleted(&root, "side");
    forget_journals(&root);
    let good = format!("{} side", side);
    let damaged = record_utils::to_bytes("tombstones", 3, [good.as_str(), "not an entry"]);
    std::fs::write(list_path(&root), &damaged).unwrap();

    let report = area.server_ok(&root, &["tombstone", "repair"]);

    assert!(report.contains("Kept 1 entry") && report.contains("1 line did not read"), "{}", report);
    assert!(!report.contains("did not match its digest"), "the list matched its digest: {}", report);
    let listed = area.server_ok(&root, &["tombstone", "list"]);
    assert!(listed.lines().any(|line| line.contains(&side) && line.contains("side")), "{}", listed);
    let aside: Vec<_> = std::fs::read_dir(root.join(".forklift")).unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("tombstones.damaged-"))
        .collect();
    assert_eq!(aside.len(), 1, "{:?}", aside);
    assert_eq!(std::fs::read(root.join(".forklift").join(&aside[0])).unwrap(), damaged, "the original is kept as it was");
}

/// A digit of a tombstone's head flipped: the list still parses, so only its digest tells. Revert:
/// read the list undigested → the server serves it, the deleted pallet's real head unheld → red;
/// or drop the unverified line → the repair reports the altered entry as kept whole → red.
#[test]
fn a_flipped_head_in_the_list_is_refused_and_its_repair_says_a_kept_entry_may_be_altered() {
    let area = Area::new("flipped");
    let root = area.served();
    let side = area.deleted(&root, "side");
    forget_journals(&root);
    let mut bytes = std::fs::read(list_path(&root)).unwrap();
    let at = bytes.windows(side.len()).position(|window| window == side.as_bytes()).unwrap();
    bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };
    std::fs::write(list_path(&root), &bytes).unwrap();

    let listed = area.server(&root, &["tombstone", "list"]);
    assert!(!listed.status.success(), "{}", String::from_utf8_lossy(&listed.stdout));
    assert!(String::from_utf8_lossy(&listed.stderr).contains("does not match its digest"),
            "{}", String::from_utf8_lossy(&listed.stderr));

    let report = area.server_ok(&root, &["tombstone", "repair"]);
    assert!(report.contains("Kept 1 entry") && report.contains("did not match its digest"), "{}", report);
    let listed = area.server_ok(&root, &["tombstone", "list"]);
    assert!(listed.contains(&side), "the real head comes back, kept with no name: {}", listed);

    // A list from before the digest has none to fail, so nothing says an entry was altered.
    // Revert: take every damaged list for one that failed its digest → a false alarm → red.
    let old = forklift_core::util::record_utils::to_bytes("tombstones", 2, [format!("{} side", side)]);
    std::fs::write(list_path(&root), old).unwrap();
    let report = area.server_ok(&root, &["tombstone", "repair"]);
    assert!(report.contains("Kept 1 entry") && !report.contains("did not match its digest"), "{}", report);
}

/// Revert: let a rebuild's write fail into heal's list of refused refs → exit 53, the damage
/// nested in a refusal whose next step is another heal → red.
#[test]
fn heal_over_damaged_tombstones_answers_with_the_repair_and_rebuilds_once_it_is_made() {
    let area = Area::new("heal");
    let root = area.served();
    area.deleted(&root, "side");
    let main = head(&root, "main");
    let main_ref = root.join(".forklift").join("pallets").join("main");
    std::fs::write(&main_ref, &main.as_bytes()[..20]).unwrap();

    let damages: [Damage; 2] = [
        ("a damaged list", |root| std::fs::write(list_path(root), b"\xff\xfe not a list").unwrap()),
        ("a lost mark", |root| forklift_core::util::file_utils::remove_file(root.join(".forklift").join("served")).unwrap()),
    ];

    for (what, damage) in damages {
        damage(&root);

        let set = format!("main={}", main);
        for args in [vec!["--json", "heal"], vec!["--json", "heal", "--ref", set.as_str()]] {
            let refused = area.run(&root, &args);
            assert_eq!(refused.status.code(), Some(TOMBSTONES_DAMAGED_EXIT), "{}: {:?}: {}{}", what, args, text(&refused.stdout), text(&refused.stderr));
            let error = json_of(&refused)["error"].clone();
            assert_eq!(error["code"], "tombstones_damaged", "{}: {}", what, error);
            assert!(error["next_step"].as_str().unwrap().starts_with("forklift-server tombstone repair --root"), "{}: {}", what, error);
            assert_eq!(std::fs::read(&main_ref).unwrap(), main.as_bytes()[..20].to_vec(), "{}: the ref is left alone", what);
        }

        // The ref whose rebuild the damage refused is said, not left out of the answer, and so is
        // a damaged ref the stop left unjudged. Revert: stop the pass with no refusal → the message
        // never names main, red; leave the first pass's unjudged refs out of heal's answer → zeta
        // unnamed, red.
        let zeta_ref = root.join(".forklift").join("pallets").join("zeta");
        std::fs::write(&zeta_ref, b"not a head\n").unwrap();
        let refused = area.run(&root, &["--json", "heal"]);
        let message = json_of(&refused)["error"]["message"].as_str().unwrap().to_string();
        let said = format!("The ref of pallet \"main\" was not rebuilt: the rebuild at {} was not attempted: the tombstones are damaged, which stop the run;", main);
        assert!(message.contains(&said), "{}: {}", what, message);
        assert!(message.contains("The ref of pallet \"zeta\" is lost or damaged; this run stopped before judging it."), "{}: {}", what, message);
        forklift_core::util::file_utils::remove_file(&zeta_ref).unwrap();

        area.server_ok(&root, &["tombstone", "repair"]);
        assert!(area.server_ok(&root, &["tombstone", "list"]).contains("side"), "{}: the repair keeps side", what);
    }

    area.run_ok(&root, &["heal"]);
    assert_eq!(head(&root, "main"), main);
}

/// A taint over the damaged ref's head leaves it for the pass after the taint recovery, which
/// meets the damaged tombstones instead: heal's answer names the ref it would rebuild, and the
/// damaged ref after it, which that pass judges all the same. Revert: break the pass at the
/// first refusal → zeta unnamed, red. The meta pass before it rebuilds a meta ref the taint held,
/// which the stop says once, after the taint.
#[test]
fn heal_over_damaged_tombstones_after_the_taint_recovery_names_every_ref_it_judged() {
    let area = Area::new("heal-after-taint");
    let root = area.served();
    area.deleted(&root, "side");
    area.run_ok(&root, &["tag", "create", "v1", "main"]);
    let main = head(&root, "main");
    let meta = root.join(".forklift").join("meta");
    let tags = std::fs::read_to_string(meta.join("tags")).unwrap().trim_end().to_string();
    std::fs::write(meta.join("tags"), &tags.as_bytes()[..20]).unwrap();
    let pallets = root.join(".forklift").join("pallets");
    std::fs::write(pallets.join("main"), &main.as_bytes()[..20]).unwrap();
    std::fs::write(pallets.join("zeta"), b"not a head\n").unwrap();
    let taint = forklift_core::util::taint_utils::taint_dir_path_under(&root);
    std::fs::create_dir_all(&taint).unwrap();
    // A staged shard the taint names that is gone is a note of the taint recovery's own.
    let shard = std::path::Path::new("inventory").join(forklift_core::util::file_utils::shard_folder_relative("gone")).join("data");
    std::fs::write(taint.join("taint-1-0"), forklift_core::util::taint_utils::taint_file_bytes(
        [
            format!("objects/{}/{}", &main[..2], &main[2..]),
            format!("objects/{}/{}", &tags[..2], &tags[2..]),
            shard.to_string_lossy().replace('\\', "/"),
        ],
    )).unwrap();
    std::fs::write(list_path(&root), b"\xff\xfe not a list").unwrap();

    let refused = area.run(&root, &["--json", "heal"]);
    assert_eq!(refused.status.code(), Some(TOMBSTONES_DAMAGED_EXIT), "{}{}", text(&refused.stdout), text(&refused.stderr));
    assert_eq!(std::fs::read_dir(&taint).map(|entries| entries.count()).unwrap_or(0), 0, "the taint recovery ran first");
    let message = json_of(&refused)["error"]["message"].as_str().unwrap().to_string();
    let said = format!("The ref of pallet \"main\" was not rebuilt: the rebuild at {} was not attempted: the tombstones are damaged, which stop the run;", main);
    assert!(message.contains(&said), "{}", message);
    assert!(message.contains("The ref of pallet \"zeta\" was not rebuilt: nothing in this warehouse names its head"), "{}", message);
    assert!(!message.contains("this run stopped before judging it"), "{}", message);
    // The taint it cleared is said too, before the notes on the run: its files are gone, so no
    // later run reports it. The shard's path is read with "/" between its parts on every platform.
    let message = message.replace('\\', "/");
    let mut restaged = [format!("objects/{}/{}", &main[..2], &main[2..]), format!("objects/{}/{}", &tags[..2], &tags[2..])];
    restaged.sort();
    let cleared = format!(
        "Durability taint cleared: restaged {}; resolved (absent and unreferenced) {}.",
        restaged.join(", "), shard.to_string_lossy().replace('\\', "/"),
    );
    let at = message.find(&cleared).unwrap_or_else(|| panic!("{}", message));
    let note = message.find("Note: ").unwrap_or_else(|| panic!("{}", message));
    assert!(at < note, "{}", message);
    // The meta ref the pass after the taint rebuilt is said once, among the repairs since it.
    let rebuilt = format!("Rebuilt the ref of pallet \"@tags\" at {}, from the ref's copy.", tags);
    assert_eq!(message.matches(&rebuilt).count(), 1, "{}", message);
    assert!(message.find(&rebuilt).is_some_and(|said| at < said && said < note), "{}", message);
    assert!(message.contains("Note: the staged inventory shard"), "{}", message);
}

/// Revert: leave kept heads out of the holder census → the redaction applies and deletes content
/// a kept head still holds → red.
#[test]
fn a_redaction_is_held_while_only_a_kept_head_holds_its_content() {
    let area = Area::new("redaction");
    // Enrolled before it is served: a served root's office moves only through its server.
    let root = area.warehouse();
    area.run_ok(&root, &["office", "enroll"]);
    mark_served(&root);
    let side = area.deleted(&root, "side");
    forget_journals(&root);
    forklift_core::util::file_utils::remove_file(list_path(&root)).unwrap();
    area.server_ok(&root, &["tombstone", "repair"]);

    area.run_ok(&root, &["redact", "record", "-m", "secret", &format!("{}:side.txt", side)]);
    area.run_ok(&root, &["redact", "apply"]);

    let listed = json_of(&area.run_ok(&root, &["--json", "redact", "list"]));
    let shown = listed.to_string();
    assert!(shown.contains(&format!("(kept head {})", side)), "{}", shown);
    assert!(!shown.contains("\"applied\""), "{}", shown);
}
