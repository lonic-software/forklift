//! What the operator binaries share (`forklift-aws-upgrade`, `forklift-aws-withhold`): the options
//! naming the table, and running a command against it from the operator's own machine.
//!
//! The table comes from `--table` or `FORKLIFT_DYNAMODB_TABLE`, the region and credentials from the
//! usual AWS environment, and `--endpoint-url` (or `FORKLIFT_AWS_ENDPOINT_URL`) points it at another
//! endpoint.

use std::process::ExitCode;

use forklift_core::error::CoreError;

use crate::aws::{build_clients, AwsConfig, DynamoOps};
use crate::AsyncBridge;

/// The table an operator binary works on, and the endpoint it reaches it at.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub endpoint_url: Option<String>,
}

/// The table options as parsing reads them, starting from the environment.
pub struct TableOptions {
    table: Option<String>,
    endpoint_url: Option<String>,
}

impl TableOptions {
    /// The options the environment gives before any argument is read.
    pub fn from_env() -> TableOptions {
        TableOptions {
            table: std::env::var("FORKLIFT_DYNAMODB_TABLE").ok(),
            endpoint_url: std::env::var("FORKLIFT_AWS_ENDPOINT_URL").ok(),
        }
    }

    /// Read `arg` if it is `--table` or `--endpoint-url`, taking its value from `args`.
    ///
    /// # Returns
    /// * `Ok(true)`    - `arg` was one of them.
    /// * `Ok(false)`   - `arg` is not a table option.
    /// * `Err(String)` - It was one, with no value after it.
    pub fn take(&mut self, arg: &str, args: &mut impl Iterator<Item = String>) -> Result<bool, String> {
        match arg {
            "--table" => self.table = Some(args.next().ok_or("--table needs a value")?),
            "--endpoint-url" => self.endpoint_url = Some(args.next().ok_or("--endpoint-url needs a value")?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// The table, once every argument is read.
    ///
    /// # Returns
    /// * `Ok(Table)`   - The table and its endpoint.
    /// * `Err(String)` - No table was named.
    pub fn resolve(self) -> Result<Table, String> {
        let name = self.table.ok_or("no table: pass --table or set FORKLIFT_DYNAMODB_TABLE")?;
        Ok(Table { name, endpoint_url: self.endpoint_url })
    }
}

/// Run `command` against `table` with a DynamoDB client and the bridge its stores block through.
///
/// The stores block on the runtime through the bridge, so `command` runs on a blocking thread,
/// never on a runtime worker.
///
/// # Returns
/// * `Ok(T)`       - What `command` returned.
/// * `Err(String)` - The runtime or the client could not be built, `command` failed, or it stopped
///                   unexpectedly.
pub fn run_on_table<T: Send + 'static>(
    table: &Table,
    command: impl FnOnce(DynamoOps, AsyncBridge) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the async runtime: {}", error))?;

    runtime.block_on(async move {
        let mut config = AwsConfig::new(String::new(), table.name.clone(), String::new());
        if let Some(endpoint) = &table.endpoint_url {
            config = config.with_endpoint_url(endpoint.clone());
        }

        let (_, dynamodb) = build_clients(&config).await?;
        let bridge = AsyncBridge::current()?;

        tokio::task::spawn_blocking(move || command(dynamodb, bridge))
            .await
            .map_err(|error| format!("the command stopped unexpectedly: {}", error))?
    })
}

/// The exit status for an operator command's `result`, printing its error for a person.
pub fn exit(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", CoreError::from(error).human_text());
            ExitCode::FAILURE
        }
    }
}
