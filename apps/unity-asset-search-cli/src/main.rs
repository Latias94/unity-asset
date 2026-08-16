mod build_identity;
mod client;
mod command;
mod json_input;
mod output;

use clap::Parser as _;
use clap::error::ErrorKind;

use unity_asset_search_protocol::{CapabilitiesRequest, RequestOperation};

use crate::client::{ConnectionOptions, execute};
use crate::command::{Action, Args};
use crate::output::{CliFailure, CliSuccess, write_failure, write_success};

#[tokio::main]
async fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(error) => {
            let failure = CliFailure::usage(error);
            let exit_code = failure.exit_code();
            let _ = write_failure(&failure);
            std::process::exit(exit_code);
        }
    };
    let result = run(args).await;
    if let Err(failure) = result {
        let exit_code = failure.exit_code();
        let _ = write_failure(&failure);
        std::process::exit(exit_code);
    }
}

async fn run(args: Args) -> Result<(), CliFailure> {
    let action = args.action()?;
    let options = ConnectionOptions::from_args(&args)?;

    match action {
        Action::DaemonStart(settings) => {
            let operation = RequestOperation::Capabilities(CapabilitiesRequest::default());
            let (binding, response) = execute(&options, Some(&settings), operation).await?;
            write_success(binding, CliSuccess::Operation(response))
        }
        Action::Operation(operation) => {
            if args.start_if_needed()
                && matches!(
                    operation,
                    unity_asset_search_protocol::RequestOperation::Shutdown(_)
                )
            {
                return Err(CliFailure::usage_message(
                    "--start-if-needed cannot be combined with daemon stop",
                ));
            }
            let start = args.start_if_needed().then_some(Default::default());
            let (binding, response) = execute(&options, start.as_ref(), operation).await?;
            write_success(binding, CliSuccess::Operation(response))
        }
    }
}
