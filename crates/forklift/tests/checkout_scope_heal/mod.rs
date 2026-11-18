//! A checkout's lost or damaged scope file is refused `checkout_scope_damaged` (exit 67), and `heal`
//! restores it from the copy every scope write leaves beside it, once that copy can be used and
//! would strand no file; `heal --scope` sets it to the paths the checkout held when it cannot.

use super::*;

const MAIN_SCOPE: &str = "sparse/.forklift/scope";
const MAIN_COPY: &str = "sparse/.forklift/scope-copy";
const BAY_SCOPE: &str = "sparse/.forklift/bays/b/scope";
const BAY_COPY: &str = "sparse/.forklift/bays/b/scope-copy";
const FETCH_SCOPE: &str = "sparse/.forklift/config/fetch-scope";
const CHECKOUT_SCOPE_DAMAGED_EXIT: i32 = 67;

/// A remote holding `src/api/a.txt`, `docs/guide.md` and `other/o.txt`, lifted from "dev", and a
/// sparse clone "sparse" fetching `docs` and `src/api`, so `other` is a subtree it never fetched.
/// Returns the server, which must outlive the test.
fn sparse_without_other(area: &TestArea) -> Server {
    let server = Server::start(area, None);

    prepare_warehouse(area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    area.write_file("dev/other/o.txt", "never fetched\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "-m", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["clone", &server.url, "sparse", "--only", "src/api", "--only", "docs"]));

    server
}

/// The same fixture with a bay "b" at "b", scoped to `src/api`.
fn with_bay(area: &TestArea) -> Server {
    let server = sparse_without_other(area);
    assert_success(&area.forklift("sparse", &["bay", "add", "b", "../b", "--scope", "src/api"]));
    server
}

/// A scope record naming `prefixes`, as a scope write leaves it (no prefix is the full scope).
fn scope_record(prefixes: &[&str]) -> Vec<u8> {
    forklift_core::util::record_utils::to_bytes("scope", 5, prefixes)
}

fn remove(area: &TestArea, path: &str) {
    forklift_core::util::file_utils::remove_file(area.path(path)).unwrap();
}

/// The `--json` refusal of `output`, asserting it is `checkout_scope_damaged` with exit 67.
fn scope_damaged(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(CHECKOUT_SCOPE_DAMAGED_EXIT), "expected checkout_scope_damaged: {}{}", stdout(output), stderr(output));
    let value = json_of(output);
    assert_eq!(value["error"]["code"], "checkout_scope_damaged", "{}", value);
    value["error"].clone()
}

/// A plain refusal of `heal --scope`, asserting it wrote no scope file.
fn refused_scope(area: &TestArea, dir: &str, scope: &str, args: &[&str]) -> String {
    let output = area.forklift(dir, &[&["heal"][..], args].concat());
    assert!(!output.status.success(), "{:?} must be refused: {}", args, stdout(&output));
    assert!(!area.path(scope).exists(), "{:?} wrote no scope", args);
    stderr(&output)
}

/// Every way a scope file is lost or damaged, in the main tree and in a bay, makes a read refuse
/// `checkout_scope_damaged` naming `forklift heal`, and heal writes it back byte for byte from its
/// copy, after which the read works. That includes a byte flipped that leaves a valid scope inside
/// the fetch scope: the main tree's narrowed to "docs/src/api".
///
/// Revert: return the old untyped refusal from the scope readers → no code, no next step → red;
/// skip the restore in heal → the scope stays damaged → red; skip the copy's re-sync → the lost
/// copy stays lost → red; read a record whose digest does not match as if it did → the main tree's
/// flipped scope reads, and `stocktake` runs narrowed → red.
#[test]
fn heal_restores_a_lost_or_damaged_scope_from_its_copy() {
    let area = TestArea::new("checkout-scope-heal-restore");
    let _server = with_bay(&area);

    type Damage<'a> = (&'a str, &'a dyn Fn(&TestArea, &str));
    let damages: [Damage; 5] = [
        ("a byte flipped", &|area, path| flip_a_scope_byte(area, path)),
        ("deleted", &|area, path| remove(area, path)),
        ("emptied", &|area, path| std::fs::write(area.path(path), b"").unwrap()),
        ("zeroed", &|area, path| {
            let length = std::fs::read(area.path(path)).unwrap().len();
            std::fs::write(area.path(path), vec![0u8; length]).unwrap();
        }),
        ("not UTF-8", &|area, path| std::fs::write(area.path(path), b"\xff\xfe").unwrap()),
    ];

    for (dir, scope, copy, expected) in [
        ("sparse", MAIN_SCOPE, MAIN_COPY, serde_json::json!(["docs", "src/api"])),
        ("b", BAY_SCOPE, BAY_COPY, serde_json::json!(["src/api"])),
    ] {
        let original = std::fs::read(area.path(scope)).unwrap();
        assert_eq!(std::fs::read(area.path(copy)).unwrap(), original, "{}: every write leaves the copy beside the scope", dir);

        for (what, damage) in &damages {
            damage(&area, scope);

            let error = scope_damaged(&area.forklift(dir, &["--json", "stocktake"]));
            assert_eq!(error["next_step"], "forklift heal", "{} {}: {}", dir, what, error);

            let healed = area.forklift(dir, &["--json", "heal"]);
            assert_success(&healed);
            let restored = &json_of(&healed)["data"]["checkout_scope_restored"];
            assert_eq!(restored["scope"], expected, "{} {}: {}", dir, what, restored);
            assert_eq!(restored["chosen"], false, "{} {}: {}", dir, what, restored);
            assert_eq!(std::fs::read(area.path(scope)).unwrap(), original, "{} {}: the scope is back exactly", dir, what);
            assert_success(&area.forklift(dir, &["stocktake"]));
        }

        // A usable scope rewrites a lost copy.
        remove(&area, copy);
        assert_success(&area.forklift(dir, &["heal"]));
        assert_eq!(std::fs::read(area.path(copy)).unwrap(), original, "{}: the copy is rewritten from the scope", dir);
    }
}

/// A copy that names a path outside the fetch scope, names no path at all, or is gone, is not
/// restored: heal refuses with no next step and names `heal --scope`.
///
/// Revert: drop the fetch-scope check on the copy → heal restores the copy that also names
/// `other`, which the warehouse never fetched → red; drop the full-scope check → with the fetch
/// scope lost too, heal restores a copy that names no path → red.
#[test]
fn heal_does_not_restore_from_a_copy_it_cannot_use() {
    let area = TestArea::new("checkout-scope-heal-bad-copy");
    let _server = sparse_without_other(&area);
    remove(&area, MAIN_SCOPE);

    // With the fetch scope lost, nothing else bounds the copy: one that names no path is still not
    // restored.
    let fetch = std::fs::read(area.path(FETCH_SCOPE)).unwrap();
    remove(&area, FETCH_SCOPE);
    std::fs::write(area.path(MAIN_COPY), scope_record(&[])).unwrap();
    let healed = area.forklift("sparse", &["--json", "heal"]);
    assert!(!area.path(MAIN_SCOPE).exists(), "full, fetch scope lost: nothing was restored");
    let message = scope_damaged(&healed)["message"].as_str().unwrap().to_string();
    assert!(message.contains("names no path"), "{}", message);
    std::fs::write(area.path(FETCH_SCOPE), fetch).unwrap();

    // A copy wider than the fetch scope strands nothing, so only the fetch-scope check stops it.
    for (what, copy, reason) in [
        ("wider than the fetch scope", Some(scope_record(&["docs", "other", "src/api"])), "outside the fetch scope"),
        ("outside the fetch scope", Some(scope_record(&["other"])), "outside the fetch scope"),
        ("full", Some(scope_record(&[])), "names no path"),
        ("deleted", None, "is missing"),
    ] {
        match copy {
            Some(bytes) => std::fs::write(area.path(MAIN_COPY), bytes).unwrap(),
            None => remove(&area, MAIN_COPY),
        }

        let healed = area.forklift("sparse", &["--json", "heal"]);
        assert!(!area.path(MAIN_SCOPE).exists(), "{}: nothing was restored", what);
        let error = scope_damaged(&healed);
        let message = error["message"].as_str().unwrap();
        assert!(error["next_step"].is_null(), "{}: no next step can be offered: {}", what, error);
        assert!(message.contains(reason) && message.contains("\"forklift heal --scope <path>...\""), "{}: {}", what, message);
    }
}

/// A copy narrower than what the checkout holds is not restored over a committed file it would
/// leave out, nor over an untracked file under a fetched path; heal refuses naming the file. Once
/// the file is gone the copy is restored, and the litter a file browser leaves in a folder the copy
/// leaves out does not stop it.
///
/// Revert: drop the stranding guard → heal restores the narrower copy → red; drop its litter
/// exemption → `docs/.DS_Store` stops the last restore → red.
#[test]
fn heal_does_not_restore_a_copy_that_would_strand_a_file() {
    let area = TestArea::new("checkout-scope-heal-strand");
    let _server = with_bay(&area);

    // The main tree holds the committed docs/guide.md, which a copy of src/api alone would strand.
    // Its configuration file is damaged in a comment, which heal mends before the refusal: the
    // refusal says so.
    remove(&area, MAIN_SCOPE);
    std::fs::write(area.path(MAIN_COPY), scope_record(&["src/api"])).unwrap();
    let config = area.path("sparse/.forklift/config/warehouse.toml");
    let mut bytes = std::fs::read(&config).unwrap();
    let at = bytes.windows(12).position(|w| w == b"pseudonymous").unwrap();
    bytes[at] = !bytes[at];
    std::fs::write(&config, bytes).unwrap();
    let error = scope_damaged(&area.forklift("sparse", &["--json", "heal"]));
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("\"docs/guide.md\""), "the committed file is named: {}", error);
    assert!(message.contains("The configuration file"), "the config rewrite is said: {}", message);
    assert!(!area.path(MAIN_SCOPE).exists());

    // The bay holds src/api only; an untracked file under the fetched docs is outside it.
    remove(&area, BAY_SCOPE);
    area.write_file("b/docs/notes.txt", "untracked\n");
    let error = scope_damaged(&area.forklift("b", &["--json", "heal"]));
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("\"docs/notes.txt\""), "the untracked file is named: {}", error);
    // It may have lain outside the checkout already, where --scope would refuse it too: moving it
    // is named, and below it works.
    assert!(message.contains("If only untracked files stand in the way, move them out of the working tree and run \"forklift heal\" again"), "{}", message);
    assert!(!area.path(BAY_SCOPE).exists());

    remove(&area, "b/docs/notes.txt");
    area.write_file("b/docs/.DS_Store", "litter\n");
    area.write_file("b/.DS_Store", "litter\n");
    area.write_file("b/src/.DS_Store", "litter\n");
    let healed = area.forklift("b", &["--json", "heal"]);
    assert_success(&healed);
    assert_eq!(json_of(&healed)["data"]["checkout_scope_restored"]["scope"], serde_json::json!(["src/api"]));
    assert_eq!(std::fs::read(area.path(BAY_SCOPE)).unwrap(), std::fs::read(area.path(BAY_COPY)).unwrap());
}

/// A scope file that reads but names a path outside the fetch scope is restored from a usable copy
/// that differs from it, and the copy is left as it was. A copy that would strand a file is not,
/// and the fetch-scope heal's refusal says so.
///
/// Revert: re-sync the copy from the scope before judging it → the copy takes the bad scope and
/// nothing is restored → red; drop the note on the stranding copy → the refusal does not say why
/// the copy was not restored → red.
#[test]
fn heal_restores_a_scope_outside_the_fetch_scope_from_its_copy() {
    let area = TestArea::new("checkout-scope-heal-outside");
    let _server = sparse_without_other(&area);
    let copy = std::fs::read(area.path(MAIN_COPY)).unwrap();

    // A copy that would strand the committed docs/guide.md does not settle it: the fetch-scope
    // heal refuses, and says why the copy was not restored.
    std::fs::write(area.path(MAIN_SCOPE), scope_record(&["other"])).unwrap();
    std::fs::write(area.path(MAIN_COPY), scope_record(&["src/api"])).unwrap();
    let refused = area.forklift("sparse", &["--json", "heal"]);
    let value = json_of(&refused);
    assert_eq!(value["error"]["code"], "fetch_scope_damaged", "{}", value);
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("so it was not restored: \"docs/guide.md\""), "{}", message);
    std::fs::write(area.path(MAIN_COPY), &copy).unwrap();

    let healed = area.forklift("sparse", &["--json", "heal"]);
    assert_success(&healed);
    let restored = &json_of(&healed)["data"]["checkout_scope_restored"];
    assert_eq!(restored["scope"], serde_json::json!(["docs", "src/api"]), "{}", restored);
    assert_eq!(std::fs::read(area.path(MAIN_SCOPE)).unwrap(), copy, "the scope is the copy's");
    assert_eq!(std::fs::read(area.path(MAIN_COPY)).unwrap(), copy, "the copy is unchanged");
    assert_success(&area.forklift("sparse", &["stocktake"]));
}

/// A scope write that cannot leave its copy fails before it touches the scope file: a folder at the
/// copy's path makes `scope narrow` refuse, and the scope and the files it covers stay as they were.
///
/// Revert: write the scope file before the copy → the scope is narrowed with no copy beside it →
/// red.
#[test]
fn a_scope_write_that_cannot_leave_its_copy_changes_nothing() {
    let area = TestArea::new("checkout-scope-copy-first");
    let _server = sparse_without_other(&area);
    let scope = std::fs::read(area.path(MAIN_SCOPE)).unwrap();

    remove(&area, MAIN_COPY);
    area.write_file(&format!("{}/blocker", MAIN_COPY), "a folder where the copy goes\n");

    let narrowed = area.forklift("sparse", &["scope", "narrow", "docs"]);
    assert!(!narrowed.status.success(), "{}", stdout(&narrowed));
    assert_eq!(std::fs::read(area.path(MAIN_SCOPE)).unwrap(), scope, "the scope is unchanged");
    assert!(area.path("sparse/docs/guide.md").exists(), "the files it covers stay");
}

/// `heal --scope` writes only a scope that is lost or damaged, and only one that holds what the
/// checkout holds: it is refused over a usable scope, too narrow (stranding a committed file), too
/// wide (a committed file with no working entry), outside a readable fetch scope, at the root, at a
/// path that is no folder, and in the main tree of a warehouse that is not sparse. The true scope
/// is then set, with its copy.
///
/// Revert: drop any one admission check → that case writes a scope, or is refused by a later check
/// for another reason → red.
#[test]
fn heal_scope_refuses_a_scope_the_checkout_does_not_hold() {
    let area = TestArea::new("checkout-scope-heal-set");
    let _server = sparse_without_other(&area);

    let usable = area.forklift("sparse", &["heal", "--scope", "docs", "--scope", "src/api"]);
    assert!(!usable.status.success() && stderr(&usable).contains("reads: it checks out"), "{}", stderr(&usable));

    remove(&area, MAIN_SCOPE);
    let narrow = refused_scope(&area, "sparse", MAIN_SCOPE, &["--scope", "src/api"]);
    assert!(narrow.contains("\"docs/guide.md\"") && narrow.contains("outside the checkout"), "{}", narrow);
    let outside = refused_scope(&area, "sparse", MAIN_SCOPE, &["--scope", "docs", "--scope", "other"]);
    assert!(outside.contains("outside the warehouse's fetch scope"), "{}", outside);
    let root = refused_scope(&area, "sparse", MAIN_SCOPE, &["--scope", "."]);
    assert!(root.contains("is the whole tree"), "{}", root);
    let file = refused_scope(&area, "sparse", MAIN_SCOPE, &["--scope", "docs/guide.md", "--scope", "src/api"]);
    assert!(file.contains("is not a folder"), "{}", file);

    // Too wide: a full warehouse's bay scoped to src/api, beside a committed src/x.txt it never
    // checked out, set to the whole of src.
    area.write_file("dev/src/x.txt", "beside the api\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "-m", "beside"]));
    assert_success(&area.forklift("dev", &["bay", "add", "w", "../w", "--scope", "src/api"]));
    remove(&area, "dev/.forklift/bays/w/scope");
    let wide = refused_scope(&area, "w", "dev/.forklift/bays/w/scope", &["--scope", "src"]);
    assert!(wide.contains("\"src/x.txt\"") && wide.contains("no working entry"), "{}", wide);
    let full = area.forklift("dev", &["heal", "--scope", "src"]);
    assert!(!full.status.success() && stderr(&full).contains("is not sparse"), "{}", stderr(&full));

    let set = area.forklift("sparse", &["--json", "heal", "--scope", "src/api", "--scope", "docs"]);
    assert_success(&set);
    let restored = &json_of(&set)["data"]["checkout_scope_restored"];
    assert_eq!(restored["chosen"], true, "{}", restored);
    assert_eq!(restored["scope"], serde_json::json!(["docs", "src/api"]), "{}", restored);
    assert_eq!(std::fs::read(area.path(MAIN_SCOPE)).unwrap(), scope_record(&["docs", "src/api"]));
    assert_eq!(std::fs::read(area.path(MAIN_COPY)).unwrap(), scope_record(&["docs", "src/api"]));
    assert_success(&area.forklift("sparse", &["stocktake"]));
}

/// With the scope, its copy and the fetch scope all gone, `heal --scope` naming the true paths sets
/// the scope, and the same heal rebuilds the fetch scope from it.
///
/// Revert: run the fetch-scope heal before the checkout's scope is set → it finds no checkout to
/// rebuild from and refuses → red.
#[test]
fn heal_scope_with_everything_lost_sets_the_scope_and_rebuilds_the_fetch_scope() {
    let area = TestArea::new("checkout-scope-heal-all-lost");
    let _server = sparse_without_other(&area);
    let fetch = std::fs::read(area.path(FETCH_SCOPE)).unwrap();
    for path in [MAIN_SCOPE, MAIN_COPY, FETCH_SCOPE] {
        remove(&area, path);
    }

    let healed = area.forklift("sparse", &["--json", "heal", "--scope", "docs", "--scope", "src/api"]);
    assert_success(&healed);
    let data = &json_of(&healed)["data"];
    assert_eq!(data["checkout_scope_restored"]["scope"], serde_json::json!(["docs", "src/api"]), "{}", data);
    assert_eq!(data["fetch_scope_rebuilt"]["scope"], serde_json::json!(["docs", "src/api"]), "{}", data);
    assert_eq!(std::fs::read(area.path(FETCH_SCOPE)).unwrap(), fetch, "the fetch scope is back as the clone wrote it");
    assert_success(&area.forklift("sparse", &["stocktake"]));
}

/// With the fetch scope gone, `heal --scope` naming a path the warehouse never fetched is refused,
/// naming the folder the store does not hold.
///
/// Revert: skip the store check → the scope takes in `other`, whose files cannot be read → red.
#[test]
fn heal_scope_refuses_a_path_the_store_does_not_hold() {
    let area = TestArea::new("checkout-scope-heal-not-held");
    let _server = sparse_without_other(&area);
    remove(&area, MAIN_SCOPE);
    remove(&area, FETCH_SCOPE);

    let refused = refused_scope(&area, "sparse", MAIN_SCOPE, &["--scope", "docs", "--scope", "src/api", "--scope", "other"]);
    assert!(refused.contains("not in the store") && refused.contains("\"other\""), "{}", refused);
}

/// The stranding and width checks judge the tree the checkout stands on, not its pallet's head: a
/// bay that shifted to the same pallet and stacked a new file moves the head past the main tree,
/// and the main tree's true scope is still set.
///
/// Revert: judge against the pallet's head → `src/api/new.txt` reads as a committed file with no
/// working entry → red.
#[test]
fn heal_scope_judges_the_tree_the_checkout_stands_on() {
    let area = TestArea::new("checkout-scope-heal-lagging");
    let _server = with_bay(&area);

    assert_success(&area.forklift("b", &["shift", "main"]));
    area.write_file("b/src/api/new.txt", "stacked by the bay\n");
    assert_success(&area.forklift("b", &["load", "."]));
    assert_success(&area.forklift("b", &["stack", "-m", "from the bay"]));
    assert!(!area.path("sparse/src/api/new.txt").exists(), "the main tree lags its pallet");

    remove(&area, MAIN_SCOPE);
    remove(&area, MAIN_COPY);
    remove(&area, FETCH_SCOPE);
    let set = area.forklift("sparse", &["--json", "heal", "--scope", "src", "--scope", "docs"]);
    assert_success(&set);
    assert_eq!(json_of(&set)["data"]["checkout_scope_restored"]["scope"], serde_json::json!(["docs", "src"]));
}

/// A bay whose `bay add` stopped before it recorded its folder checked nothing out: a read of its
/// damaged scope from another checkout names `forklift bay remove`, not a heal it has no folder for.
///
/// Revert: offer heal for every other bay, in the reader or in the fetch-scope heal's listing → the
/// message names a heal in a folder that does not exist → red.
#[test]
fn a_pathless_bay_with_a_damaged_scope_is_removed_not_healed() {
    let area = TestArea::new("checkout-scope-pathless-bay");
    let _server = with_bay(&area);
    remove(&area, "sparse/.forklift/bays/b/path");
    remove(&area, BAY_SCOPE);

    let refused = area.forklift("sparse", &["--json", "scope", "prune", "docs"]);
    let error = scope_damaged(&refused);
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("\"forklift bay remove b\""), "{}", message);
    assert!(!message.contains("forklift heal"), "{}", message);
    assert!(error["next_step"].is_null(), "{}", error);

    // The fetch-scope heal lists it with the same way out.
    remove(&area, FETCH_SCOPE);
    let healed = area.forklift("sparse", &["--json", "heal"]);
    let said = format!("{}{}", stdout(&healed), stderr(&healed));
    assert!(said.contains("forklift bay remove b"), "{}", said);
}

/// With the position and the fetch scope lost as well, heal rebuilds the position first, whose base
/// the stranding guard reads, then restores the scope from its copy, then rebuilds the fetch scope
/// from the checkouts' scopes, this one's included.
///
/// Revert: skip the early position heal → the guard cannot read the position and heal refuses →
/// red; leave the position and the scope for after the fetch-scope heal → its rebuild from bay "b"
/// alone leaves out "docs", the copy then lies outside it and is refused → red.
#[test]
fn heal_restores_the_position_and_scope_before_the_fetch_scope() {
    let area = TestArea::new("checkout-scope-heal-after-position");
    let _server = with_bay(&area);
    let scope = std::fs::read(area.path(MAIN_SCOPE)).unwrap();
    remove(&area, MAIN_SCOPE);
    remove(&area, "sparse/.forklift/position");
    remove(&area, FETCH_SCOPE);

    let healed = area.forklift("sparse", &["--json", "heal"]);
    assert_success(&healed);
    let data = &json_of(&healed)["data"];
    assert_eq!(data["checkout_scope_restored"]["scope"], serde_json::json!(["docs", "src/api"]), "{}", data);
    assert!(!data["position_rebuilt"].is_null(), "{}", data);
    assert_eq!(data["fetch_scope_rebuilt"]["scope"], serde_json::json!(["docs", "src/api"]), "{}", data);
    assert_eq!(std::fs::read(area.path(MAIN_SCOPE)).unwrap(), scope);
    assert_success(&area.forklift("sparse", &["stocktake"]));
}

// A --position whose pallet's ref is damaged too, the way the position refusal steers: the
// position waits for the --ref write, but the scope is still restored before the fetch-scope
// heal, judged against the parcel that --ref names. Revert: judge it against the lost position →
// the scope heal refuses: what it would strand cannot be told.
#[test]
fn heal_position_beside_its_ref_restores_the_scope_before_the_fetch_scope() {
    let area = TestArea::new("checkout-scope-heal-position-ref");
    let _server = with_bay(&area);
    let scope = std::fs::read(area.path(MAIN_SCOPE)).unwrap();
    let ref_file = "sparse/.forklift/pallets/main";
    let head = std::fs::read_to_string(area.path(ref_file)).unwrap().trim().to_string();
    remove(&area, MAIN_SCOPE);
    remove(&area, "sparse/.forklift/position");
    remove(&area, "sparse/.forklift/position-copy");
    remove(&area, FETCH_SCOPE);
    std::fs::write(area.path(ref_file), &head.as_bytes()[..20]).unwrap();

    let healed = area.forklift("sparse", &["--json", "heal", "--position", "main", "--ref", &format!("main={}", head)]);
    assert_success(&healed);
    let data = &json_of(&healed)["data"];
    assert_eq!(data["checkout_scope_restored"]["scope"], serde_json::json!(["docs", "src/api"]), "{}", data);
    assert_eq!(data["position_rebuilt"]["base"], head, "{}", data);
    assert_eq!(data["fetch_scope_rebuilt"]["scope"], serde_json::json!(["docs", "src/api"]), "{}", data);
    assert_eq!(std::fs::read(area.path(MAIN_SCOPE)).unwrap(), scope);
    assert_success(&area.forklift("sparse", &["stocktake"]));
}

// A --position that heal will refuse, since the position still reads, is refused before the scope
// is judged against the parcel its --ref names. Revert: decide from the flags alone → the scope is
// restored against that parcel, then --position is refused.
#[test]
fn heal_refuses_a_position_it_will_not_set_before_restoring_the_scope() {
    let area = TestArea::new("checkout-scope-heal-position-reads");
    let _server = with_bay(&area);
    let head = std::fs::read_to_string(area.path("sparse/.forklift/pallets/main")).unwrap().trim().to_string();
    remove(&area, MAIN_SCOPE);

    let refused = area.forklift("sparse", &["heal", "--position", "main", "--ref", &format!("main={}", head)]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(!area.path(MAIN_SCOPE).exists(), "no scope was written: {}", stderr(&refused));
    assert!(stderr(&refused).contains("--position sets only a position that is lost"), "{}", stderr(&refused));
}

// A position a newer version wrote cannot be read, so the tree the checkout stands on cannot be
// told: heal and heal --scope both refuse with the code, and write nothing.
#[test]
fn a_base_that_cannot_be_read_is_a_coded_refusal() {
    let area = TestArea::new("checkout-scope-heal-base-unread");
    let _server = sparse_without_other(&area);
    let head = std::fs::read_to_string(area.path("sparse/.forklift/pallets/main")).unwrap().trim().to_string();
    let newer = forklift_core::util::record_utils::to_bytes("position", 99, ["main", &head]);
    std::fs::write(area.path("sparse/.forklift/position"), &newer).unwrap();
    remove(&area, MAIN_SCOPE);

    for args in [&["--json", "heal"][..], &["--json", "heal", "--scope", "docs", "--scope", "src/api"][..]] {
        let output = area.forklift("sparse", args);
        let value = json_of(&output);
        assert_eq!(value["error"]["code"], "checkout_scope_damaged", "{:?}: {}", args, value);
        assert_eq!(output.status.code(), Some(CHECKOUT_SCOPE_DAMAGED_EXIT), "{:?}", args);
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("cannot be told") && message.contains("until the position and that tree both read"), "{:?}: {}", args, message);
        assert!(!area.path(MAIN_SCOPE).exists(), "{:?} wrote no scope", args);
    }
}

// A --position that waits for a ref write heal will refuse is refused before the scope is judged
// against the tree that write would give it: a --ref of a ref that is not damaged, a pallet named
// twice, and a --remove-ref of its own pallet while another pallet remains. Revert (the first):
// only resolve the parcel → the scope is restored against it, then the --ref write is refused.
#[test]
fn heal_refuses_a_ref_it_will_not_set_before_restoring_the_scope() {
    let area = TestArea::new("checkout-scope-heal-ref-refused");
    let _server = with_bay(&area);
    let head = std::fs::read_to_string(area.path("sparse/.forklift/pallets/main")).unwrap().trim().to_string();
    assert_success(&area.forklift("sparse", &["pallet", "create", "other"]));
    remove(&area, MAIN_SCOPE);
    remove(&area, "sparse/.forklift/position");
    remove(&area, "sparse/.forklift/position-copy");

    let set = format!("main={}", head);
    for (args, reason) in [
        (vec!["heal", "--position", "main", "--ref", &set], "is not damaged"),
        (vec!["heal", "--position", "main", "--ref", &set, "--remove-ref", "main"], "is named more than once by --ref and --remove-ref: name each pallet once"),
        (vec!["heal", "--position", "main", "--remove-ref", "main"], "name one of \"b\", \"other\" with --position instead"),
    ] {
        let refused = area.forklift("sparse", &args);
        assert!(!refused.status.success(), "{:?}: {}", args, stdout(&refused));
        assert!(!area.path(MAIN_SCOPE).exists(), "{:?}: no scope was written: {}", args, stderr(&refused));
        assert!(stderr(&refused).contains(reason), "{:?}: {}", args, stderr(&refused));
    }
}
