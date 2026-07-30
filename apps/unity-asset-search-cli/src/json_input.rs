use std::fs::File;
use std::io;
use std::path::Path;

use serde::Deserialize;
use unity_asset_core::{
    AssetLoadBudget, ContractJsonLimits, ContractJsonResourceModel, read_contract_json,
};
use unity_asset_search_protocol::{RequestOperation, ValidateContract};

use crate::output::{CLI_CONTRACT_VERSION, CliFailure};

const MAX_CLI_REQUEST_BYTES: usize = 512 * 1024;
const CLI_REQUEST_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "unity_asset_search_cli_request_v1",
    MAX_CLI_REQUEST_BYTES,
    32,
    65_536,
    65_536,
    ContractJsonResourceModel::new(7, 4 * 1024, 16 * 1024, 512),
);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CliRequestV1 {
    cli_contract_version: u16,
    operation: RequestOperation,
}

pub fn read_operation(path: &Path) -> Result<RequestOperation, CliFailure> {
    let mut budget = AssetLoadBudget::default();
    let request: CliRequestV1 = if path == Path::new("-") {
        let stdin = io::stdin();
        read_contract_json(stdin.lock(), &mut budget, CLI_REQUEST_LIMITS)
    } else {
        let file = File::open(path).map_err(|error| {
            CliFailure::input(format!("open request JSON {}: {error}", path.display()))
        })?;
        read_contract_json(file, &mut budget, CLI_REQUEST_LIMITS)
    }
    .map_err(|error| CliFailure::input(format!("invalid bounded request JSON: {error}")))?;

    if request.cli_contract_version != CLI_CONTRACT_VERSION {
        return Err(CliFailure::input(format!(
            "unsupported CLI request contract version {}; expected {}",
            request.cli_contract_version, CLI_CONTRACT_VERSION
        )));
    }
    request
        .operation
        .validate()
        .map_err(|error| CliFailure::input(format!("invalid request operation: {error}")))?;
    Ok(request.operation)
}
