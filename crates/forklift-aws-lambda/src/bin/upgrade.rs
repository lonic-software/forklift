//! `forklift-aws-upgrade`: upgrade the warehouses of an AWS deployment in place to the format this
//! build reads ([`forklift_aws_lambda::format`]). An operator runs it from their own machine, under
//! a role granting `iam/operator.policy.json`, never from a Lambda.
//!
//! ```text
//! forklift-aws-upgrade --table <table> (--all | <warehouse>...) [--takeover] [--wait-secs <n>]
//! ```
//!
//! The table comes from `--table` or `FORKLIFT_DYNAMODB_TABLE`, the region and credentials from the
//! usual AWS environment, and `--endpoint-url` (or `FORKLIFT_AWS_ENDPOINT_URL`) points it at another
//! endpoint. Each warehouse is upgraded in turn; one that fails is reported and the rest are still
//! upgraded, since every warehouse not yet upgraded is refused. The exit status is non-zero if any
//! failed. `--takeover` takes over the upgrades on record of the warehouses it names, so it is never
//! given with `--all`: a run still upgrading some other warehouse would be taken over too.

use std::process::ExitCode;
use std::time::Duration;

use forklift_aws_lambda::aws::dynamo::list_warehouses;
use forklift_aws_lambda::aws::build_clients;
use forklift_aws_lambda::format::{self, UpgradeOptions, CHAIN};
use forklift_aws_lambda::reissue::request_token;
use forklift_aws_lambda::{AsyncBridge, AwsConfig, DynamoRefStore};
use forklift_core::error::CoreError;

/// The wait after marking a warehouse, by default: well past the control-plane Lambda's timeout,
/// which `infra/aws-serverless` caps at API Gateway's 29 seconds. The verifier never reads a
/// warehouse's partition, so its timeout does not count.
const DEFAULT_WAIT_SECS: u64 = 60;

const USAGE: &str = "usage: forklift-aws-upgrade [--table <table>] [--endpoint-url <url>] \
    (--all | <warehouse>...) [--takeover] [--wait-secs <seconds>]";

struct Arguments {
    table: String,
    endpoint_url: Option<String>,
    all: bool,
    warehouses: Vec<String>,
    takeover: bool,
    wait: Duration,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut table = std::env::var("FORKLIFT_DYNAMODB_TABLE").ok();
    let mut endpoint_url = std::env::var("FORKLIFT_AWS_ENDPOINT_URL").ok();
    let mut all = false;
    let mut warehouses = Vec::new();
    let mut takeover = false;
    let mut wait = Duration::from_secs(DEFAULT_WAIT_SECS);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--table" => table = Some(args.next().ok_or("--table needs a value")?),
            "--endpoint-url" => endpoint_url = Some(args.next().ok_or("--endpoint-url needs a value")?),
            "--all" => all = true,
            "--takeover" => takeover = true,
            "--wait-secs" => {
                let value = args.next().ok_or("--wait-secs needs a value")?;
                wait = Duration::from_secs(value.parse().map_err(|_| format!("--wait-secs takes whole seconds, not {:?}", value))?);
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {}", flag)),
            warehouse => warehouses.push(warehouse.to_string()),
        }
    }

    let table = table.ok_or("no table: pass --table or set FORKLIFT_DYNAMODB_TABLE")?;

    if all && takeover {
        return Err("--takeover takes over only the warehouses it names; it is never given with --all".to_string());
    }

    match (all, warehouses.is_empty()) {
        (true, false) => Err("name warehouses or pass --all, not both".to_string()),
        (false, true) => Err("name the warehouses to upgrade, or pass --all".to_string()),
        _ => Ok(Arguments { table, endpoint_url, all, warehouses, takeover, wait }),
    }
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

    // The stores block on the runtime through the bridge, so the upgrade runs on a blocking
    // thread, never on a runtime worker.
    let result = runtime.block_on(async move {
        let mut config = AwsConfig::new(String::new(), arguments.table.clone(), String::new());
        if let Some(endpoint) = &arguments.endpoint_url {
            config = config.with_endpoint_url(endpoint.clone());
        }

        let (_, dynamodb) = build_clients(&config).await?;
        let bridge = AsyncBridge::current()?;

        tokio::task::spawn_blocking(move || run(&arguments, dynamodb, bridge))
            .await
            .map_err(|error| format!("the upgrade stopped unexpectedly: {}", error))?
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", CoreError::from(error).human_text());
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &Arguments, dynamodb: forklift_aws_lambda::aws::DynamoOps, bridge: AsyncBridge) -> Result<(), String> {
    let warehouses = if arguments.all {
        list_warehouses(&dynamodb, &arguments.table, &bridge)?
    } else {
        arguments.warehouses.clone()
    };

    if warehouses.is_empty() {
        println!("The table holds no warehouses. Nothing to do.");
        return Ok(());
    }

    let options = UpgradeOptions { owner: request_token(), takeover: arguments.takeover, wait: arguments.wait };
    let mut failed = 0;

    // A run that stops part way leaves its mark naming this token, and a later run's refusal names
    // it back: the operator tells their own run from someone else's by it.
    println!("This is run {}.", options.owner);

    for warehouse in &warehouses {
        let store = DynamoRefStore::new(
            dynamodb.clone(),
            arguments.table.clone(),
            warehouse.clone(),
            String::new(),
            bridge.clone(),
        );

        match format::upgrade(&store, &CHAIN, warehouse, &options) {
            Ok(report) => {
                for line in report.summary() {
                    println!("{}", line);
                }
            }
            Err(error) => {
                eprintln!("{}", CoreError::from(error).human_text());
                failed += 1;
            }
        }
    }

    match failed {
        0 => Ok(()),
        _ => Err(format!("{} of {} warehouses were not upgraded.", failed, warehouses.len())),
    }
}
