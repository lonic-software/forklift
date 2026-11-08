//! `forklift-aws-withhold`: withhold a file's content in a warehouse of an AWS deployment, as its
//! host, or list what is withheld there ([`forklift_aws_lambda::withheld`]). An operator runs it
//! from their own machine, under a role granting `iam/operator.policy.json`, never from a Lambda.
//!
//! ```text
//! forklift-aws-withhold --table <table> <warehouse> add <hash> --statement <text>
//! forklift-aws-withhold --table <table> <warehouse> list
//! ```
//!
//! The table comes from `--table` or `FORKLIFT_DYNAMODB_TABLE`, the region and credentials from the
//! usual AWS environment, and `--endpoint-url` (or `FORKLIFT_AWS_ENDPOINT_URL`) points it at another
//! endpoint. The role reads no object, so `add` records the hash with its statement and the head
//! works out the rest: what the hash is held as, on the next request, and what to delete.

use std::process::ExitCode;

use forklift_aws_lambda::aws::build_clients;
use forklift_aws_lambda::withheld::{self, Added, WithheldItem};
use forklift_aws_lambda::{AsyncBridge, AwsConfig, DynamoRefStore};
use forklift_core::error::CoreError;

const USAGE: &str = "usage: forklift-aws-withhold [--table <table>] [--endpoint-url <url>] <warehouse> \
    (add <hash> --statement <text> | list)";

enum Command {
    Add { hash: String, statement: String },
    List,
}

struct Arguments {
    table: String,
    endpoint_url: Option<String>,
    warehouse: String,
    command: Command,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut table = std::env::var("FORKLIFT_DYNAMODB_TABLE").ok();
    let mut endpoint_url = std::env::var("FORKLIFT_AWS_ENDPOINT_URL").ok();
    let mut statement = None;
    let mut words = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--table" => table = Some(args.next().ok_or("--table needs a value")?),
            "--endpoint-url" => endpoint_url = Some(args.next().ok_or("--endpoint-url needs a value")?),
            "--statement" => statement = Some(args.next().ok_or("--statement needs a value")?),
            flag if flag.starts_with('-') => return Err(format!("unknown option {}", flag)),
            word => words.push(word.to_string()),
        }
    }

    let table = table.ok_or("no table: pass --table or set FORKLIFT_DYNAMODB_TABLE")?;

    let (warehouse, command) = match (words.as_slice(), statement) {
        ([warehouse, add, hash], Some(statement)) if add == "add" => {
            (warehouse.clone(), Command::Add { hash: hash.clone(), statement })
        }
        ([_, add, _], None) if add == "add" => return Err("add needs --statement <text>".to_string()),
        ([warehouse, list], None) if list == "list" => (warehouse.clone(), Command::List),
        ([_, list], Some(_)) if list == "list" => return Err("list takes no --statement".to_string()),
        _ => return Err("name a warehouse, then add <hash> or list".to_string()),
    };

    Ok(Arguments { table, endpoint_url, warehouse, command })
}

fn main() -> ExitCode {
    let arguments = match parse(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("{}\n{}", error, USAGE);
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the async runtime: {}", error);
            return ExitCode::FAILURE;
        }
    };

    // The store blocks on the runtime through the bridge, so the command runs on a blocking
    // thread, never on a runtime worker.
    let result = runtime.block_on(async move {
        let mut config = AwsConfig::new(String::new(), arguments.table.clone(), String::new());
        if let Some(endpoint) = &arguments.endpoint_url {
            config = config.with_endpoint_url(endpoint.clone());
        }

        let (_, dynamodb) = build_clients(&config).await?;
        let bridge = AsyncBridge::current()?;

        tokio::task::spawn_blocking(move || {
            let store = DynamoRefStore::new(
                dynamodb,
                arguments.table.clone(),
                arguments.warehouse.clone(),
                String::new(),
                bridge,
            );
            run(&arguments, &store)
        })
        .await
        .map_err(|error| format!("the command stopped unexpectedly: {}", error))?
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", CoreError::from(error).human_text());
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &Arguments, store: &DynamoRefStore) -> Result<(), String> {
    match &arguments.command {
        Command::Add { hash, statement } => {
            let line = match withheld::add(store, hash, statement)? {
                Added::Added => "is now withheld",
                Added::Restated => "was withheld already; its statement is now the one given",
                Added::Unchanged => "was withheld already, with this statement",
            };
            println!("{} {} in {}.", hash, line, arguments.warehouse);
        }
        Command::List => {
            let listed = withheld::list(store)?;

            if listed.is_empty() {
                println!("Nothing is withheld in {}.", arguments.warehouse);
            }

            for entry in listed {
                println!("{}  {}  {}", entry.hash, progress(&entry.item), entry.item.statement);
            }
        }
    }

    Ok(())
}

/// How far the head has carried a withholding out, in a word or two.
fn progress(item: &WithheldItem) -> &'static str {
    match item {
        WithheldItem { never_applies: true, .. } => "never applies (not a file's content)",
        WithheldItem { deleted: true, .. } => "deleted",
        WithheldItem { swept: true, kept: true, .. } => "withheld, copy kept for history",
        WithheldItem { swept: true, .. } => "withheld, deleting",
        WithheldItem { settled: true, .. } => "withheld, deletion owed",
        _ => "withheld, not yet seen by the head",
    }
}
