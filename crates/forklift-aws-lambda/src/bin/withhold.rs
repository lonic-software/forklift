//! `forklift-aws-withhold`: withhold a file's content in a warehouse of an AWS deployment, as its
//! host, or list what is withheld there ([`forklift_aws_lambda::withheld`]). An operator runs it
//! from their own machine, under a role granting `iam/operator.policy.json`, never from a Lambda.
//!
//! ```text
//! forklift-aws-withhold --table <table> <warehouse> add <hash> --statement <text>
//! forklift-aws-withhold --table <table> <warehouse> list
//! ```
//!
//! The table and its endpoint are named as for every operator binary
//! ([`forklift_aws_lambda::aws::operator`]). The role reads no object, so `add` records the hash
//! with its statement and nothing more: what the store holds the hash as, and what to delete, is
//! the head's to work out. The head refuses the content from its next request on; this build's
//! head does not yet delete the copies it holds, so `add` says so.

use std::process::ExitCode;

use forklift_aws_lambda::aws::operator::{self, run_on_table, Table, TableOptions};
use forklift_aws_lambda::withheld::{self, Added, WithheldItem};
use forklift_aws_lambda::DynamoRefStore;

const USAGE: &str = "usage: forklift-aws-withhold [--table <table>] [--endpoint-url <url>] <warehouse> \
    (add <hash> --statement <text> | list)";

enum Command {
    Add { hash: String, statement: String },
    List,
}

struct Arguments {
    table: Table,
    warehouse: String,
    command: Command,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut table = TableOptions::from_env();
    let mut statement = None;
    let mut words = Vec::new();

    while let Some(arg) = args.next() {
        if table.take(&arg, &mut args)? {
            continue;
        }

        match arg.as_str() {
            "--statement" => statement = Some(args.next().ok_or("--statement needs a value")?),
            flag if flag.starts_with('-') => return Err(format!("unknown option {}", flag)),
            word => words.push(word.to_string()),
        }
    }

    let table = table.resolve()?;

    let (warehouse, command) = match (words.as_slice(), statement) {
        ([warehouse, add, hash], Some(statement)) if add == "add" => {
            (warehouse.clone(), Command::Add { hash: hash.clone(), statement })
        }
        ([_, add, _], None) if add == "add" => return Err("add needs --statement <text>".to_string()),
        ([warehouse, list], None) if list == "list" => (warehouse.clone(), Command::List),
        ([_, list], Some(_)) if list == "list" => return Err("list takes no --statement".to_string()),
        _ => return Err("name a warehouse, then add <hash> or list".to_string()),
    };

    Ok(Arguments { table, warehouse, command })
}

fn main() -> ExitCode {
    let arguments = match parse(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("{}\n{}", error, USAGE);
            return ExitCode::from(2);
        }
    };

    operator::exit(run_on_table(arguments.table.clone(), move |dynamodb, bridge| {
        let store = DynamoRefStore::new(
            dynamodb,
            arguments.table.name.clone(),
            arguments.warehouse.clone(),
            String::new(),
            bridge,
        );
        run(&arguments, &store)
    }))
}

fn run(arguments: &Arguments, store: &DynamoRefStore) -> Result<(), String> {
    match &arguments.command {
        Command::Add { hash, statement } => {
            let added = withheld::add(store, hash, statement)?;
            let line = match added {
                Added::Added => "is now recorded as withheld",
                Added::Restated => "was recorded as withheld already; its statement is now the one given",
                Added::Unchanged => "was recorded as withheld already, with this statement",
                Added::Uncertain => "is recorded as withheld, with this statement",
            };
            println!("{} {} in {}.", hash, line, arguments.warehouse);
            if added == Added::Uncertain {
                println!("A write's fate was unknown, so whether this run recorded it cannot be told.");
            }
            println!("The head refuses the content from its next request on. This build's head does not yet delete the copies it holds.");
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
        _ => "recorded",
    }
}
