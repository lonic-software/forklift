//! The parcel query surface: the verified-by-default trust ordering, the predicate bounds,
//! pagination, and the missing-parcel hard error.
//!
//! The headline test constructs the exact attack the design exists to catch: a parcel whose
//! *recorded* author is a human while its *signature* is an agent's key (a cherry-pick
//! produces this shape natively — the original author is preserved as a recorded action,
//! the picker signs). A verified `query --class agent` must report that parcel (the signer
//! is what's forge-proof); a recorded query must show the divergence the other way. If the
//! engine ever prunes on the recorded value before verifying, this test fails.

use std::process::{Command, Output};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

struct TestWarehouse {
    root: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl TestWarehouse {
    fn new(name: &str) -> TestWarehouse {
        let root = std::env::temp_dir()
            .join(format!("forklift-test-{}-{}", name, std::process::id()));
        let home = std::env::temp_dir()
            .join(format!("forklift-test-{}-{}-home", name, std::process::id()));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        TestWarehouse { root, home }
    }

    fn write_file(&self, relative_path: &str, content: &str) {
        let path = self.root.join(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(FORKLIFT)
            .args(args)
            .current_dir(&self.root)
            // Tests must never read or write the developer's real global configuration.
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .output()
            .unwrap()
    }

    /// Run a command feeding `input` on stdin (for `--where -`: an oversized predicate
    /// cannot ride argv — Windows caps a command line at ~32 KB, far below the 64 KiB
    /// payload bound this suite has to overflow).
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = Command::new(FORKLIFT)
            .args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();

        child.wait_with_output().unwrap()
    }

    /// Delete a parcel object (and its signature sidecar) from the loose store.
    fn delete_parcel(&self, hash: &str) {
        let objects = self.root.join(".forklift").join("objects").join(&hash[0..2]);
        std::fs::remove_file(objects.join(&hash[2..])).expect("the parcel object existed");
        let _ = std::fs::remove_file(objects.join(format!("{}.sig", &hash[2..])));
    }
}

impl Drop for TestWarehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|error| {
        panic!("not JSON ({}): {}", error, stdout(output))
    })
}

fn configure_operator(warehouse: &TestWarehouse) {
    assert_success(&warehouse.run(&["config", "--global", "operator.name", "Test Operator"]));
    assert_success(&warehouse.run(&["config", "--global", "operator.identifier", "test@forklift"]));
}

/// Generate a keypair for `operator_id` (switching the warehouse operator to it and back
/// to the "test@forklift" admin) and return the printed admit args:
/// `[operator_id, public_key, pop]`.
fn keygen_admit_args(warehouse: &TestWarehouse, operator_id: &str) -> Vec<String> {
    assert_success(&warehouse.run(&["config", "operator.identifier", operator_id]));
    let keygen = stdout(&warehouse.run(&["office", "keygen"]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .skip(2)
        .map(|token| token.to_string())
        .collect()
}

fn extract_parcel_hash(stack_output: &Output) -> String {
    let text = stdout(stack_output);
    let line = text.lines().find(|line| line.contains("Stacked parcel"))
        .unwrap_or_else(|| panic!("no 'Stacked parcel' line in: {}", text));

    line.split_whitespace().nth(2).unwrap().to_string()
}

/// A signed warehouse with a human admin (`test@forklift`) and a supervised agent
/// (`agent@forklift`). Returns after enrolling both; the current operator is the human.
fn signed_warehouse_with_agent(name: &str) -> TestWarehouse {
    let warehouse = TestWarehouse::new(name);
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base parcel"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    let agent = keygen_admit_args(&warehouse, "agent@forklift");
    assert_success(&warehouse.run(&[
        "office", "admit", &agent[0], &agent[1], &agent[2],
        "--agent", "--supervisor", "test@forklift",
    ]));

    warehouse
}

/// The parcels a query reported, as (hash, author-trust, author-operator) triples.
fn matches_of(report: &serde_json::Value) -> Vec<(String, String, Option<String>)> {
    report["data"]["matches"]
        .as_array()
        .expect("matches is an array")
        .iter()
        .map(|entry| {
            (
                entry["parcel"].as_str().unwrap().to_string(),
                entry["author"]["trust"].as_str().unwrap().to_string(),
                entry["author"]["operator"].as_str().map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn verified_query_filters_on_the_signer_never_the_recorded_author() {
    let warehouse = signed_warehouse_with_agent("query-two-phase");

    // An ordinary agent parcel: authored, stacked and signed by the agent.
    assert_success(&warehouse.run(&["config", "operator.identifier", "agent@forklift"]));
    warehouse.write_file("agent.txt", "by the agent\n");
    assert_success(&warehouse.run(&["load", "agent.txt"]));
    let agent_parcel = extract_parcel_hash(&warehouse.run(&["stack", "agent work"]));

    // The forgery shape (the design's crux finding): a parcel whose *recorded* author is
    // the human but whose *signature* is the agent's key. A cherry-pick produces exactly
    // this — the original author rides along as a recorded action; the picker signs.
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));
    assert_success(&warehouse.run(&["palletize", "drafts"]));
    warehouse.write_file("draft.txt", "human draft\n");
    assert_success(&warehouse.run(&["load", "draft.txt"]));
    let human_draft = extract_parcel_hash(&warehouse.run(&["stack", "human draft work"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    assert_success(&warehouse.run(&["config", "operator.identifier", "agent@forklift"]));
    let pick = warehouse.run(&["cherry-pick", &human_draft]);
    assert_success(&pick);
    let picked = stdout(&pick)
        .split("stacked parcel ")
        .nth(1)
        .expect("cherry-pick names the stacked parcel")
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    // Verified (the default): --class agent resolves the SIGNER. The picked parcel is
    // agent-signed, so it matches — even though its recorded author is the human. Pruning
    // on the recorded value would have dropped it before verification; this is the test
    // that the two-phase ordering exists.
    let verified = json(&warehouse.run(&["--json", "query", "main", "--class", "agent"]));
    let reported = matches_of(&verified);
    assert!(
        reported.iter().any(|(hash, trust, operator)| {
            hash == &picked && trust == "verified" && operator.as_deref() == Some("agent@forklift")
        }),
        "the agent-signed cherry-pick must match a verified --class agent: {:?}",
        reported
    );
    assert!(
        reported.iter().any(|(hash, _, _)| hash == &agent_parcel),
        "the ordinary agent parcel matches too: {:?}",
        reported
    );
    assert!(
        !reported.iter().any(|(hash, _, _)| hash == &human_draft),
        "the human-signed draft (on the other pallet's line, but also human-signed) must \
         not match: {:?}",
        reported
    );

    // The evasion direction: agent-signed work cannot pass as human. --class human must
    // exclude the picked parcel no matter what its actions record.
    let humans = json(&warehouse.run(&["--json", "query", "main", "--class", "human"]));
    assert!(
        !matches_of(&humans).iter().any(|(hash, _, _)| hash == &picked),
        "an agent-signed parcel must never satisfy a verified --class human"
    );

    // Recorded trust (the labeled opt-out) answers from the parcel's own claim: the picked
    // parcel reads as human-authored there — visibly labeled, and visibly different.
    let recorded = json(&warehouse.run(&["--json", "query", "main", "--recorded", "--class", "agent"]));
    let reported = matches_of(&recorded);
    assert!(
        !reported.iter().any(|(hash, _, _)| hash == &picked),
        "recorded trust reads the recorded (human) author: {:?}",
        reported
    );
    assert!(
        reported.iter().all(|(_, trust, _)| trust == "recorded"),
        "every recorded-mode answer is labeled: {:?}",
        reported
    );
    assert_eq!(recorded["data"]["scope"]["trust"], "recorded");

    // The supervised agent never matches --unsupervised.
    let unsupervised = json(&warehouse.run(&["--json", "query", "main", "--unsupervised"]));
    assert_eq!(unsupervised["data"]["matches"].as_array().unwrap().len(), 0);

    // The scope block is always present, and the office join is honest about its reach.
    assert_eq!(verified["data"]["scope"]["trust"], "verified");
    assert_eq!(verified["data"]["scope"]["office_asof"], "current");

    // history --json now stamps its (recorded) trust tier on every action, so the same
    // flag word on the two commands can never silently read as the same guarantee.
    let history = json(&warehouse.run(&["--json", "history"]));
    let action = &history["data"]["entries"][0]["actions"][0];
    assert_eq!(action["trust"], "recorded");
}

#[test]
fn predicate_bounds_and_malformed_predicates_refuse_with_exit_18() {
    let warehouse = TestWarehouse::new("query-bounds");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base parcel"]));

    // Depth: 17 nested "not"s. Leaves: 129 in an "all". In-array: 257 values. Glob: 257
    // chars. Payload: over 64 KiB. Malformed: not JSON / unknown field / wrong op.
    let depth_bomb = format!(
        "{}{}{}",
        "{\"not\":".repeat(17),
        "{\"field\":\"is_merge\",\"op\":\"eq\",\"value\":true}",
        "}".repeat(17)
    );
    let leaves = (0..129)
        .map(|_| "{\"field\":\"is_merge\",\"op\":\"eq\",\"value\":true}")
        .collect::<Vec<_>>()
        .join(",");
    let leaf_bomb = format!("{{\"all\":[{}]}}", leaves);
    let in_values = (0..257).map(|i| format!("\"{}\"", i)).collect::<Vec<_>>().join(",");
    let in_bomb = format!("{{\"field\":\"author.operator\",\"op\":\"in\",\"value\":[{}]}}", in_values);
    let glob_bomb = format!(
        "{{\"field\":\"description\",\"op\":\"matches\",\"value\":\"{}\"}}",
        "x".repeat(257)
    );
    for payload in [
        depth_bomb.as_str(),
        leaf_bomb.as_str(),
        in_bomb.as_str(),
        glob_bomb.as_str(),
        "not json",
        "{\"field\":\"provenance.model\",\"op\":\"eq\",\"value\":\"x\"}",
        "{\"field\":\"is_merge\",\"op\":\"matches\",\"value\":\"x\"}",
    ] {
        let output = warehouse.run(&["--json", "query", "--where", payload]);
        assert_eq!(
            output.status.code(),
            Some(18),
            "payload must refuse with exit 18: {}...",
            &payload[..payload.len().min(80)]
        );
        let error = json(&output);
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["code"], "query_predicate_invalid", "for: {}", payload);
    }

    // The payload byte bound goes through stdin (`--where -`): a 65 KiB argument would
    // overflow Windows' ~32 KB command line before forklift ever saw it.
    let payload_bomb = format!(
        "{{\"field\":\"description\",\"op\":\"matches\",\"value\":\"{}\"}}",
        "x".repeat(65 * 1024)
    );
    let output = warehouse.run_with_stdin(&["--json", "query", "--where", "-"], &payload_bomb);
    assert_eq!(output.status.code(), Some(18), "the payload byte bound refuses with exit 18");
    assert_eq!(json(&output)["error"]["code"], "query_predicate_invalid");

    // Signer predicates have no recorded-trust fallback: refused up front, same code.
    let output = warehouse.run(&["--json", "query", "--recorded", "--signer", "abc"]);
    assert_eq!(output.status.code(), Some(18));

    // A plain bad flag stays clap's exit 2, unrelated to the predicate taxonomy.
    let output = warehouse.run(&["query", "--no-such-flag"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn query_pages_deterministically_with_a_cursor() {
    let warehouse = TestWarehouse::new("query-pages");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "parcel 0"]));

    for index in 1..6 {
        warehouse.write_file("a.txt", &format!("content {}\n", index));
        assert_success(&warehouse.run(&["load", "a.txt"]));
        assert_success(&warehouse.run(&[&"stack".to_string(), &format!("parcel {}", index)]));
    }

    let full = json(&warehouse.run(&["--json", "query"]));
    let all_hashes: Vec<String> =
        matches_of(&full).into_iter().map(|(hash, _, _)| hash).collect();
    assert_eq!(all_hashes.len(), 6);

    // Read the same history two matches at a time; the concatenation must reproduce the
    // unpaged order exactly, with no duplicates and no gaps.
    let mut paged: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut args: Vec<String> =
            vec!["--json".into(), "query".into(), "-n".into(), "2".into()];
        if let Some(cursor) = &cursor {
            args.push("--after".into());
            args.push(cursor.clone());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let page = json(&warehouse.run(&arg_refs));

        paged.extend(matches_of(&page).into_iter().map(|(hash, _, _)| hash));

        match page["data"]["next"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }

    assert_eq!(paged, all_hashes, "paged reads reproduce the unpaged walk exactly");
}

#[test]
fn a_missing_parcel_body_errors_the_whole_query() {
    let warehouse = TestWarehouse::new("query-missing-parcel");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let first = extract_parcel_hash(&warehouse.run(&["stack", "first parcel"]));

    warehouse.write_file("a.txt", "two\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    assert_success(&warehouse.run(&["stack", "second parcel"]));

    // A parcel body is spine, not sparse content: deleting it must hard-error the query
    // (like audit), never degrade to a soft count an attacker could hide a deletion in.
    warehouse.delete_parcel(&first);

    let output = warehouse.run(&["--json", "query"]);
    assert!(!output.status.success(), "a missing parcel body must error the query");
    assert_eq!(json(&output)["ok"], false);
}

#[test]
fn an_empty_pallet_answers_honestly_empty() {
    let warehouse = TestWarehouse::new("query-empty");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let report = json(&warehouse.run(&["--json", "query"]));
    assert_eq!(report["ok"], true);
    assert_eq!(report["data"]["matches"].as_array().unwrap().len(), 0);
    assert_eq!(report["data"]["scope"]["walked"], 0);
}
