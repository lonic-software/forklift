//! `heal` rebuilds a sparse warehouse's lost or damaged fetch scope from its checkouts' scopes, once
//! the history there is all present, and refuses `fetch_scope_damaged` (exit 54) when they cannot
//! settle it; `heal --fetch-scope` then sets it, fetching from the origin what the history lacks.

use super::*;

const FETCH_SCOPE: &str = "sparse/.forklift/config/fetch-scope";
const MAIN_SCOPE: &str = "sparse/.forklift/scope";
const FETCH_SCOPE_DAMAGED_EXIT: i32 = 54;

/// The `--json` refusal of `output`, asserting it is `fetch_scope_damaged` with exit 54.
fn fetch_scope_damaged(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(FETCH_SCOPE_DAMAGED_EXIT), "expected fetch_scope_damaged: {}{}", stdout(output), stderr(output));
    let value = json_of(output);
    assert_eq!(value["error"]["code"], "fetch_scope_damaged", "{}", value);
    value["error"].clone()
}

/// Run a refusal's next step, a whole `forklift …` command, in `dir`.
fn run_next_step(area: &TestArea, dir: &str, error: &serde_json::Value) -> Output {
    let step = error["next_step"].as_str().unwrap_or_else(|| panic!("no next step: {}", error));
    let words: Vec<&str> = step.split(' ').collect();
    assert_eq!(words[0], "forklift", "the next step is a forklift command: {}", step);
    area.forklift(dir, &[&["--json"][..], &words[1..]].concat())
}

fn written_fetch_scope(area: &TestArea) -> Vec<u8> {
    std::fs::read(area.path(FETCH_SCOPE)).unwrap()
}

/// Every way the fetch-scope file is lost or damaged (deleted, emptied, zeroed, made invalid UTF-8)
/// makes `load` refuse naming `heal`, and `heal` rebuilds it from the main tree's scope, exactly as
/// the clone wrote it, after which `load` works.
///
/// Revert: keep `REMEDY_REFRANCHISE` on read_fetch_state, or drop `scope_heal::repair` from heal →
/// the refusal names no heal, or heal exits 0 with the file still damaged → red.
#[test]
fn heal_rebuilds_a_lost_or_damaged_fetch_scope_from_the_checkouts() {
    let area = TestArea::new("fetch-scope-heal-rebuild");
    let _server = sparse_with_docs(&area);
    let original = written_fetch_scope(&area);

    let damages: [(&str, &dyn Fn(&TestArea)); 4] = [
        ("deleted", &|area| forklift_core::util::file_utils::remove_file(area.path(FETCH_SCOPE)).unwrap()),
        ("emptied", &|area| std::fs::write(area.path(FETCH_SCOPE), b"").unwrap()),
        ("zeroed", &|area| std::fs::write(area.path(FETCH_SCOPE), vec![0u8; original.len()]).unwrap()),
        ("not UTF-8", &|area| {
            let mut bytes = std::fs::read(area.path(FETCH_SCOPE)).unwrap();
            bytes[3] = 0xff;
            std::fs::write(area.path(FETCH_SCOPE), bytes).unwrap();
        }),
    ];

    for (what, damage) in damages {
        damage(&area);

        let refused = area.forklift("sparse", &["--json", "load", "."]);
        assert!(!refused.status.success(), "{}: load must refuse", what);
        let message = json_of(&refused)["error"]["message"].as_str().unwrap().to_string();
        assert!(message.contains("\"forklift heal\""), "{}: the refusal names heal: {}", what, message);

        let healed = area.forklift("sparse", &["--json", "heal"]);
        assert_success(&healed);
        let rebuilt = &json_of(&healed)["data"]["fetch_scope_rebuilt"];
        assert_eq!(rebuilt["scope"], serde_json::json!(["docs", "src/api"]), "{}: {}", what, rebuilt);
        assert_eq!(rebuilt["chosen"], false, "{}", what);
        assert_eq!(written_fetch_scope(&area), original, "{}: the clone's own bytes are back", what);

        assert_success(&area.forklift("sparse", &["load", "."]));
        let again = area.forklift("sparse", &["--json", "heal"]);
        assert_success(&again);
        assert!(json_of(&again)["data"]["fetch_scope_rebuilt"].is_null(), "{}: a sound scope is left alone", what);
    }
}

/// A main tree narrowed below what was fetched rebuilds a narrower fetch scope, and the report
/// says what that leaves unchecked and how to add a path back.
///
/// Revert: take the rebuilt scope from anything but the checkouts' union → red.
#[test]
fn heal_rebuilds_the_union_and_says_what_it_no_longer_checks() {
    let area = TestArea::new("fetch-scope-heal-narrowed");
    let _server = sparse_with_docs(&area);
    let bay_dir = area.path("sparse-bay");
    assert_success(&area.forklift("sparse", &["bay", "add", "b", bay_dir.to_str().unwrap(), "--scope", "docs"]));
    assert_success(&area.forklift("sparse", &["scope", "narrow", "docs"]));

    forklift_core::util::file_utils::remove_file(area.path(FETCH_SCOPE)).unwrap();

    let healed = area.forklift("sparse", &["heal"]);
    assert_success(&healed);
    let text = stdout(&healed);
    assert!(text.contains("rebuilt as \"docs\", \"src/api\""), "{}", text);
    assert!(text.contains("no longer checked for loss") && text.contains("forklift scope fetch"), "{}", text);

    // With the bay's scope lost too, only the main tree's narrowed one is left.
    forklift_core::util::file_utils::remove_file(area.path(FETCH_SCOPE)).unwrap();
    forklift_core::util::file_utils::remove_file(area.path("sparse/.forklift/bays/b/scope")).unwrap();
    let healed = area.forklift("sparse", &["--json", "heal"]);
    assert_success(&healed);
    let rebuilt = &json_of(&healed)["data"]["fetch_scope_rebuilt"];
    assert_eq!(rebuilt["scope"], serde_json::json!(["src/api"]), "{}", rebuilt);
    assert_eq!(rebuilt["left_out"][0]["checkout"], "bay \"b\"", "{}", rebuilt);
}

/// An object lost under the union stops the rebuild: heal refuses 54, writes nothing, and its next
/// step fetches the object back from the origin and sets the scope.
///
/// Revert: let the check of the history at the union pass whatever it finds → heal exits 0 over
/// the lost object → red.
#[test]
fn heal_refuses_a_rebuild_over_lost_history_and_its_next_step_fetches_it() {
    let area = TestArea::new("fetch-scope-heal-lost");
    let _server = sparse_with_docs(&area);
    let base = pallet_head(&area, "sparse", "main");
    let guide = path_object_hash(&area, "sparse", &base, "docs/guide.md");

    forklift_core::util::file_utils::remove_file(object_store_path(&area, "sparse", &guide)).unwrap();
    forklift_core::util::file_utils::remove_file(area.path(FETCH_SCOPE)).unwrap();

    let error = fetch_scope_damaged(&area.forklift("sparse", &["--json", "heal"]));
    assert!(!area.path(FETCH_SCOPE).exists(), "nothing was written");
    assert_eq!(error["next_step"], "forklift heal --fetch-scope docs src/api", "{}", error);
    assert!(error["details"]["missing"].as_str().is_some_and(|missing| !missing.is_empty()), "{}", error);

    let healed = run_next_step(&area, "sparse", &error);
    assert_success(&healed);
    let rebuilt = &json_of(&healed)["data"]["fetch_scope_rebuilt"];
    assert_eq!(rebuilt["chosen"], true, "{}", rebuilt);
    assert!(rebuilt["fetched_objects"].as_u64().is_some_and(|count| count >= 1), "{}", rebuilt);
    assert!(object_store_path(&area, "sparse", &guide).exists(), "the lost object is fetched back");
    assert_success(&area.forklift("sparse", &["load", "."]));
}

/// With no checkout's scope readable, heal refuses 54 offering the working tree's folders, and that
/// step works with the fetch-scope file still deleted. Offline the same step refuses and writes
/// nothing.
///
/// Revert: build the client through `content_may_be_absent`, which reads the damaged file → the
/// step cannot dial → red.
#[test]
fn heal_with_no_readable_checkout_offers_a_fetch_scope_that_works() {
    let area = TestArea::new("fetch-scope-heal-no-checkout");
    let mut server = sparse_with_docs(&area);

    forklift_core::util::file_utils::remove_file(area.path(FETCH_SCOPE)).unwrap();
    std::fs::write(area.path(MAIN_SCOPE), b"\xff\xfe").unwrap();

    let error = fetch_scope_damaged(&area.forklift("sparse", &["--json", "heal"]));
    assert_eq!(error["next_step"], "forklift heal --fetch-scope docs src", "{}", error);
    assert!(error["details"]["unread"][0]["reason"].as_str().unwrap().contains("UTF-8"), "{}", error);

    server.stop();
    let offline = run_next_step(&area, "sparse", &error);
    assert!(!offline.status.success(), "offline the step refuses: {}", stdout(&offline));
    assert!(!area.path(FETCH_SCOPE).exists(), "nothing was written offline");

    server.restart(&area);

    let healed = run_next_step(&area, "sparse", &error);
    assert_success(&healed);
    assert_eq!(json_of(&healed)["data"]["fetch_scope_rebuilt"]["scope"], serde_json::json!(["docs", "src"]));
}

/// A fetch scope that parses but does not hold a checkout's scope (a flipped byte kept it valid)
/// is refused with both ways out; choosing the fetch scope as the damaged one fetches and sets it.
///
/// Revert: skip the check of the checkouts against a readable fetch scope → heal exits 0 → red.
#[test]
fn heal_refuses_a_fetch_scope_that_does_not_hold_a_checkout() {
    let area = TestArea::new("fetch-scope-heal-outside");
    let _server = sparse_with_docs(&area);
    std::fs::write(area.path(FETCH_SCOPE), "/forklift-scope 4\ndocs\nsrc/apj\n").unwrap();

    let error = fetch_scope_damaged(&area.forklift("sparse", &["--json", "heal"]));
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("forklift bay remove") && message.contains("clone the remote"), "both ways out: {}", message);
    assert_eq!(error["next_step"], "forklift heal --fetch-scope docs src/api src/apj", "{}", error);
    assert_eq!(error["details"]["outside"], serde_json::json!(["the warehouse's own working tree"]), "{}", error);

    let healed = run_next_step(&area, "sparse", &error);
    assert_success(&healed);
    assert_eq!(json_of(&healed)["data"]["fetch_scope_rebuilt"]["scope"], serde_json::json!(["docs", "src/api", "src/apj"]));
    assert_success(&area.forklift("sparse", &["load", "."]));
}

/// `--fetch-scope` is refused, writing nothing, when it does not hold a checkout's scope, names the
/// root, or the fetch scope is sound; and in a full warehouse.
///
/// Revert: drop any of those admission checks → that case writes a scope → red.
#[test]
fn heal_fetch_scope_refuses_what_it_must_not_write() {
    let area = TestArea::new("fetch-scope-heal-admission");
    let _server = sparse_with_docs(&area);
    let original = written_fetch_scope(&area);

    let sound = area.forklift("sparse", &["heal", "--fetch-scope", "docs", "src/api"]);
    assert!(!sound.status.success() && stderr(&sound).contains("leaves it alone"), "{}", stderr(&sound));
    assert_eq!(written_fetch_scope(&area), original);

    forklift_core::util::file_utils::remove_file(area.path(FETCH_SCOPE)).unwrap();

    let narrower = area.forklift("sparse", &["heal", "--fetch-scope", "docs"]);
    assert!(!narrower.status.success(), "{}", stdout(&narrower));
    assert!(stderr(&narrower).contains("\"forklift heal --fetch-scope docs src/api\""), "{}", stderr(&narrower));
    assert!(!area.path(FETCH_SCOPE).exists());

    let root = area.forklift("sparse", &["heal", "--fetch-scope", "."]);
    assert!(!root.status.success() && stderr(&root).contains("cannot be the warehouse root"), "{}", stderr(&root));
    assert!(!area.path(FETCH_SCOPE).exists());

    let full = area.forklift("dev", &["heal", "--fetch-scope", "docs"]);
    assert!(!full.status.success() && stderr(&full).contains("has nothing to set"), "{}", stderr(&full));
}
