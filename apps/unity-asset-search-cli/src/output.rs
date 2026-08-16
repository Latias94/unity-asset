use std::io::{self, Write};

use serde::Serialize;
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, DaemonInstanceId, ProjectId, QueryPolicyId, ResponseOperation,
};

use crate::client::EndpointBinding;

pub const CLI_CONTRACT_VERSION: u16 = 2;
const LOCAL_ERROR_SOURCE: &str = "unity_asset_search_cli";
const MAX_LOCAL_MESSAGE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    Usage,
    Input,
    Unavailable,
    Transport,
    Protocol,
    Daemon,
    Internal,
}

impl FailureCategory {
    const fn exit_code(self) -> i32 {
        match self {
            Self::Usage => 2,
            Self::Input => 3,
            Self::Unavailable => 4,
            Self::Transport => 5,
            Self::Protocol => 6,
            Self::Daemon => 7,
            Self::Internal => 70,
        }
    }
}

#[derive(Debug)]
pub struct CliFailure {
    category: FailureCategory,
    error: Box<ApiError>,
}

impl CliFailure {
    pub fn usage(error: clap::Error) -> Self {
        Self::usage_message(error.to_string())
    }

    pub fn usage_message(message: impl Into<String>) -> Self {
        Self::local(
            FailureCategory::Usage,
            ApiErrorCode::InvalidRequest,
            message,
            false,
        )
    }

    pub fn input(message: impl Into<String>) -> Self {
        Self::local(
            FailureCategory::Input,
            ApiErrorCode::InvalidRequest,
            message,
            false,
        )
    }

    pub fn unavailable(message: impl Into<String>, retryable: bool) -> Self {
        Self::local(
            FailureCategory::Unavailable,
            ApiErrorCode::NotReady,
            message,
            retryable,
        )
    }

    pub fn transport(message: impl Into<String>, retryable: bool) -> Self {
        Self::local(
            FailureCategory::Transport,
            ApiErrorCode::PeerRejected,
            message,
            retryable,
        )
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::local(
            FailureCategory::Protocol,
            ApiErrorCode::IncompatibleProtocol,
            message,
            false,
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::local(
            FailureCategory::Internal,
            ApiErrorCode::Internal,
            message,
            false,
        )
    }

    pub fn daemon(error: ApiError) -> Self {
        Self {
            category: FailureCategory::Daemon,
            error: Box::new(error),
        }
    }

    fn local(
        category: FailureCategory,
        code: ApiErrorCode,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        let message = truncate_utf8(message.into(), MAX_LOCAL_MESSAGE_BYTES);
        let error = ApiError::new(code, message, retryable)
            .with_detail("source", LOCAL_ERROR_SOURCE)
            .with_detail("category", category_name(category));
        Self {
            category,
            error: Box::new(error),
        }
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.category.exit_code()
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CliSuccess {
    Operation(Box<ResponseOperation>),
}

#[derive(Serialize)]
struct SuccessDocument {
    cli_contract_version: u16,
    project_id: ProjectId,
    daemon_instance_id: DaemonInstanceId,
    query_policy_id: QueryPolicyId,
    result: CliSuccess,
}

#[derive(Serialize)]
struct FailureDocument<'a> {
    cli_contract_version: u16,
    category: FailureCategory,
    error: &'a ApiError,
}

pub fn write_success(binding: EndpointBinding, result: CliSuccess) -> Result<(), CliFailure> {
    let document = SuccessDocument {
        cli_contract_version: CLI_CONTRACT_VERSION,
        project_id: binding.project_id,
        daemon_instance_id: binding.daemon_instance_id,
        query_policy_id: binding.query_policy_id,
        result,
    };
    write_json(io::stdout().lock(), &document)
        .map_err(|error| CliFailure::internal(format!("write CLI success JSON: {error}")))
}

pub fn write_failure(failure: &CliFailure) -> io::Result<()> {
    let document = FailureDocument {
        cli_contract_version: CLI_CONTRACT_VERSION,
        category: failure.category,
        error: failure.error.as_ref(),
    };
    write_json(io::stderr().lock(), &document)
}

fn write_json(mut writer: impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn truncate_utf8(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

const fn category_name(category: FailureCategory) -> &'static str {
    match category {
        FailureCategory::Usage => "usage",
        FailureCategory::Input => "input",
        FailureCategory::Unavailable => "unavailable",
        FailureCategory::Transport => "transport",
        FailureCategory::Protocol => "protocol",
        FailureCategory::Daemon => "daemon",
        FailureCategory::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_utf8;

    #[test]
    fn local_message_truncation_preserves_utf8() {
        let value = format!("{}x", "界".repeat(4));
        let truncated = truncate_utf8(value, 10);
        assert_eq!(truncated, "界界界");
    }
}
