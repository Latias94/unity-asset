use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::time::Duration;

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use unity_asset_core::ObjectAddress;
use unity_asset_search_protocol::{
    CapabilitiesRequest, FilesystemReindexIntent, OperationId, PortablePath, ReferenceCursor,
    ReferenceDirection, ReferenceRequest, ReferenceSelector, ReindexAdmitRequest,
    ReindexCancelRequest, ReindexStatusRequest, ReindexWaitRequest, RequestOperation,
    SearchRequest, ShutdownRequest, StatusRequest, SuggestRequest, ValidateContract,
};

use crate::json_input;
use crate::output::CliFailure;

const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Parser)]
#[command(
    name = "unity-asset-search-cli",
    version = crate::build_identity::VERSION_REPORT,
    about
)]
pub struct Args {
    #[arg(long, global = true, value_name = "UNITY_PROJECT")]
    project_root: Option<PathBuf>,

    #[arg(long, global = true, value_name = "INDEX_DIR")]
    index_dir: Option<PathBuf>,

    #[arg(long, global = true, value_name = "EXECUTABLE")]
    daemon_binary: Option<PathBuf>,

    #[arg(long, global = true)]
    start_if_needed: bool,

    #[arg(
        long,
        global = true,
        default_value_t = NonZeroU64::new(DEFAULT_CONNECT_TIMEOUT_MS).expect("nonzero default")
    )]
    connect_timeout_ms: NonZeroU64,

    #[arg(
        long,
        global = true,
        default_value_t = NonZeroU64::new(DEFAULT_REQUEST_TIMEOUT_MS).expect("nonzero default")
    )]
    request_timeout_ms: NonZeroU64,

    #[arg(long, global = true, value_name = "PATH_OR_DASH")]
    request_json: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

impl Args {
    pub fn action(&self) -> Result<Action, CliFailure> {
        match (&self.request_json, &self.command) {
            (Some(_), Some(_)) => Err(CliFailure::usage_message(
                "--request-json cannot be combined with a subcommand",
            )),
            (None, None) => Err(CliFailure::usage_message(
                "provide a subcommand or --request-json <path|->",
            )),
            (Some(path), None) => json_input::read_operation(path).map(Action::Operation),
            (None, Some(command)) => command.lower(),
        }
    }

    pub fn project_root(&self) -> Result<PathBuf, CliFailure> {
        match &self.project_root {
            Some(path) => Ok(path.clone()),
            None => std::env::current_dir()
                .map_err(|error| CliFailure::input(format!("resolve current directory: {error}"))),
        }
    }

    #[must_use]
    pub fn index_dir(&self) -> Option<PathBuf> {
        self.index_dir.clone()
    }

    #[must_use]
    pub fn daemon_binary(&self) -> Option<PathBuf> {
        self.daemon_binary.clone()
    }

    #[must_use]
    pub const fn start_if_needed(&self) -> bool {
        self.start_if_needed
    }

    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms.get())
    }

    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms.get())
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify endpoint identity and negotiate protocol capabilities.
    Bootstrap,
    Capabilities,
    Status,
    Search(SearchArgs),
    Suggest(SuggestArgs),
    References(ReferencesArgs),
    Reindex {
        #[command(subcommand)]
        command: ReindexCommand,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

impl Command {
    fn lower(&self) -> Result<Action, CliFailure> {
        let operation = match self {
            Self::Bootstrap => return Ok(Action::Bootstrap),
            Self::Capabilities => RequestOperation::Capabilities(CapabilitiesRequest::default()),
            Self::Status => RequestOperation::Status(StatusRequest::default()),
            Self::Search(args) => RequestOperation::Search(SearchRequest {
                query: args.query.clone(),
                limit: args.limit,
            }),
            Self::Suggest(args) => RequestOperation::Suggest(SuggestRequest {
                prefix: args.prefix.clone(),
                limit: args.limit,
            }),
            Self::References(args) => RequestOperation::References(args.request()?),
            Self::Reindex { command } => command.operation()?,
            Self::Daemon {
                command: DaemonCommand::Start(args),
            } => return Ok(Action::DaemonStart(args.settings())),
            Self::Daemon {
                command: DaemonCommand::Attach,
            } => return Ok(Action::Bootstrap),
            Self::Daemon {
                command: DaemonCommand::Stop(args),
            } => RequestOperation::Shutdown(ShutdownRequest {
                drain_timeout_ms: args.drain_timeout_ms,
            }),
        };
        operation
            .validate()
            .map_err(|error| CliFailure::input(format!("invalid command request: {error}")))?;
        Ok(Action::Operation(operation))
    }
}

#[derive(Debug)]
pub enum Action {
    Bootstrap,
    DaemonStart(DaemonStartSettings),
    Operation(RequestOperation),
}

#[derive(Debug, ClapArgs)]
struct SearchArgs {
    query: String,
    #[arg(long, default_value_t = 25)]
    limit: u32,
}

#[derive(Debug, ClapArgs)]
struct SuggestArgs {
    prefix: String,
    #[arg(long, default_value_t = 10)]
    limit: u32,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DirectionArg {
    Incoming,
    Outgoing,
}

impl From<DirectionArg> for ReferenceDirection {
    fn from(value: DirectionArg) -> Self {
        match value {
            DirectionArg::Incoming => Self::Incoming,
            DirectionArg::Outgoing => Self::Outgoing,
        }
    }
}

#[derive(Debug, ClapArgs)]
#[command(group(
    clap::ArgGroup::new("selector")
        .required(true)
        .multiple(false)
        .args(["object", "guid"])
))]
struct ReferencesArgs {
    #[arg(long, value_enum, default_value_t = DirectionArg::Incoming)]
    direction: DirectionArg,
    #[arg(long, value_name = "OBJECT_ADDRESS")]
    object: Option<String>,
    #[arg(long, value_name = "GUID")]
    guid: Option<String>,
    #[arg(long)]
    file_id: Option<i64>,
    #[arg(long, default_value_t = 25)]
    limit: u32,
    #[arg(long, value_name = "CURSOR_JSON")]
    cursor: Option<String>,
}

impl ReferencesArgs {
    fn request(&self) -> Result<ReferenceRequest, CliFailure> {
        if self.object.is_some() && self.file_id.is_some() {
            return Err(CliFailure::input(
                "--file-id is valid only with the GUID selector",
            ));
        }
        let selector = match (&self.object, &self.guid) {
            (Some(address), None) => ReferenceSelector::Object {
                address: ObjectAddress::from_str(address).map_err(|error| {
                    CliFailure::input(format!("invalid object address: {error}"))
                })?,
            },
            (None, Some(guid)) => ReferenceSelector::Guid {
                guid: guid.clone(),
                file_id: self.file_id,
            },
            _ => {
                return Err(CliFailure::usage_message(
                    "references requires exactly one of --object or --guid",
                ));
            }
        };
        let cursor = self.cursor.as_deref().map(parse_cursor).transpose()?;
        let request = ReferenceRequest {
            direction: self.direction.into(),
            selector,
            limit: self.limit,
            cursor,
        };
        request
            .validate()
            .map_err(|error| CliFailure::input(format!("invalid references request: {error}")))?;
        Ok(request)
    }
}

fn parse_cursor(value: &str) -> Result<ReferenceCursor, CliFailure> {
    if value.len() > 64 * 1024 {
        return Err(CliFailure::input(
            "reference cursor JSON exceeds 65536 bytes",
        ));
    }
    let cursor: ReferenceCursor = serde_json::from_str(value)
        .map_err(|error| CliFailure::input(format!("invalid reference cursor JSON: {error}")))?;
    cursor
        .validate()
        .map_err(|error| CliFailure::input(format!("invalid reference cursor: {error}")))?;
    Ok(cursor)
}

#[derive(Debug, Subcommand)]
enum ReindexCommand {
    Admit(ReindexAdmitArgs),
    Status(OperationArgs),
    Wait(ReindexWaitArgs),
    Cancel(OperationArgs),
}

impl ReindexCommand {
    fn operation(&self) -> Result<RequestOperation, CliFailure> {
        match self {
            Self::Admit(args) => Ok(RequestOperation::ReindexAdmit(args.request()?)),
            Self::Status(args) => Ok(RequestOperation::ReindexStatus(ReindexStatusRequest {
                operation_id: args.operation_id()?,
            })),
            Self::Wait(args) => Ok(RequestOperation::ReindexWait(ReindexWaitRequest {
                operation_id: args.operation.operation_id()?,
                timeout_ms: args.timeout_ms,
            })),
            Self::Cancel(args) => Ok(RequestOperation::ReindexCancel(ReindexCancelRequest {
                operation_id: args.operation_id()?,
            })),
        }
    }
}

#[derive(Debug, ClapArgs)]
struct ReindexAdmitArgs {
    #[arg(long, conflicts_with_all = ["reconcile", "path"])]
    full: bool,
    #[arg(long, conflicts_with_all = ["full", "path"])]
    reconcile: bool,
    #[arg(long = "path", value_name = "RELATIVE_PATH", conflicts_with_all = ["full", "reconcile"])]
    path: Vec<PathBuf>,
    #[arg(long)]
    idempotency_key: Option<String>,
}

impl ReindexAdmitArgs {
    fn request(&self) -> Result<ReindexAdmitRequest, CliFailure> {
        let intent = if self.full {
            FilesystemReindexIntent::full()
        } else if !self.path.is_empty() {
            let mut paths = self
                .path
                .iter()
                .map(|path| {
                    let portable = PortablePath::from_path(path).map_err(|error| {
                        CliFailure::input(format!(
                            "reindex path {} is not portable UTF-8: {error}",
                            path.display()
                        ))
                    })?;
                    portable.require_relative().map_err(|error| {
                        CliFailure::input(format!(
                            "reindex path {} must be project-relative: {error}",
                            path.display()
                        ))
                    })?;
                    Ok(portable)
                })
                .collect::<Result<Vec<_>, CliFailure>>()?;
            paths.sort();
            paths.dedup();
            FilesystemReindexIntent::changed_paths(paths)
        } else {
            let _ = self.reconcile;
            FilesystemReindexIntent::reconcile()
        };
        let request = ReindexAdmitRequest {
            intent,
            idempotency_key: self.idempotency_key.clone(),
        };
        request
            .validate()
            .map_err(|error| CliFailure::input(format!("invalid reindex request: {error}")))?;
        Ok(request)
    }
}

#[derive(Debug, ClapArgs)]
struct OperationArgs {
    operation_id: String,
}

impl OperationArgs {
    fn operation_id(&self) -> Result<OperationId, CliFailure> {
        OperationId::from_str(&self.operation_id)
            .map_err(|error| CliFailure::input(format!("invalid operation ID: {error}")))
    }
}

#[derive(Debug, ClapArgs)]
struct ReindexWaitArgs {
    #[command(flatten)]
    operation: OperationArgs,
    #[arg(long, default_value_t = 60_000)]
    timeout_ms: u32,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Start(DaemonStartArgs),
    Attach,
    Stop(DaemonStopArgs),
}

#[derive(Debug, ClapArgs)]
struct DaemonStartArgs {
    #[arg(long)]
    no_watch: bool,
    #[arg(long)]
    no_startup_reindex: bool,
    #[arg(long)]
    scan_all: bool,
}

impl DaemonStartArgs {
    fn settings(&self) -> DaemonStartSettings {
        DaemonStartSettings {
            watch: !self.no_watch,
            startup_reindex: !self.no_startup_reindex,
            scan_all: self.scan_all,
        }
    }
}

#[derive(Debug, ClapArgs)]
struct DaemonStopArgs {
    #[arg(long, default_value_t = 30_000)]
    drain_timeout_ms: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct DaemonStartSettings {
    pub watch: bool,
    pub startup_reindex: bool,
    pub scan_all: bool,
}

impl Default for DaemonStartSettings {
    fn default() -> Self {
        Self {
            watch: true,
            startup_reindex: true,
            scan_all: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser as _;
    use unity_asset_core::{DigestV1, ObjectAddress, SourceLocator};
    use unity_asset_search_protocol::{
        GenerationIdV1, OperationKind, QueryPolicyId, ReferenceCursor, ReferenceDirection,
        ReferenceRequest, ReferenceSelector, RequestOperation,
    };

    use super::{Action, Args};
    use crate::json_input;
    use crate::output::CLI_CONTRACT_VERSION;

    const GUID: &str = "0123456789abcdef0123456789abcdef";
    const OPERATION_ID: &str = "operation-v1:11111111111111111111111111111111";

    #[test]
    fn every_protocol_operation_has_equivalent_flag_and_json_lowering() {
        let cases = [
            (vec!["capabilities"], OperationKind::Capabilities),
            (vec!["status"], OperationKind::Status),
            (vec!["search", "player"], OperationKind::Search),
            (vec!["suggest", "pla"], OperationKind::Suggest),
            (
                vec!["references", "--guid", GUID],
                OperationKind::References,
            ),
            (
                vec!["reindex", "admit", "--full"],
                OperationKind::ReindexAdmit,
            ),
            (
                vec!["reindex", "status", OPERATION_ID],
                OperationKind::ReindexStatus,
            ),
            (
                vec!["reindex", "wait", OPERATION_ID],
                OperationKind::ReindexWait,
            ),
            (
                vec!["reindex", "cancel", OPERATION_ID],
                OperationKind::ReindexCancel,
            ),
            (vec!["daemon", "stop"], OperationKind::Shutdown),
        ];

        for (arguments, expected_kind) in cases {
            let operation = lower_operation(&arguments);
            assert_eq!(operation.kind(), expected_kind);
            let directory = tempfile::tempdir().expect("temporary request directory");
            let path = directory.path().join("request.json");
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "cli_contract_version": CLI_CONTRACT_VERSION,
                    "operation": &operation,
                }))
                .expect("serialize CLI request"),
            )
            .expect("write CLI request");
            assert_eq!(
                json_input::read_operation(&path).expect("read CLI request"),
                operation
            );
        }
    }

    #[test]
    fn reference_flags_cover_directions_selectors_and_bound_cursor() {
        let address = ObjectAddress::binary_direct(
            SourceLocator::path("Assets/Data.asset").expect("source locator"),
            17,
        )
        .expect("object address")
        .to_compact_string()
        .expect("compact address");
        let object = lower_operation(&[
            "references",
            "--direction",
            "outgoing",
            "--object",
            &address,
            "--limit",
            "500",
        ]);
        assert!(matches!(
            object,
            RequestOperation::References(ReferenceRequest {
                direction: ReferenceDirection::Outgoing,
                selector: ReferenceSelector::Object { .. },
                limit: 500,
                cursor: None,
            })
        ));

        let base = ReferenceRequest::outgoing_guid(GUID, Some(42), 25);
        let cursor = ReferenceCursor {
            generation: GenerationIdV1::new(DigestV1::from_bytes([0x66; 32])),
            query_policy_id: QueryPolicyId::from_bytes([0x44; 32]),
            after_stable_id: "reference:page-1".to_owned(),
            query_binding: base.cursor_query_binding().expect("cursor binding"),
        };
        let cursor_json = serde_json::to_string(&cursor).expect("cursor JSON");
        let with_cursor = lower_operation(&[
            "references",
            "--direction",
            "outgoing",
            "--guid",
            GUID,
            "--file-id",
            "42",
            "--cursor",
            &cursor_json,
        ]);
        assert_eq!(
            with_cursor,
            RequestOperation::References(base.with_cursor(cursor))
        );
    }

    #[test]
    fn command_limits_preserve_zero_and_maximum_protocol_behavior() {
        let zero_search = lower_operation(&["search", "", "--limit", "0"]);
        assert!(matches!(
            zero_search,
            RequestOperation::Search(ref request) if request.limit == 0
        ));
        assert!(try_lower(&["search", "q", "--limit", "1001"]).is_err());
        assert!(try_lower(&["suggest", "q", "--limit", "0"]).is_err());
        assert!(try_lower(&["suggest", "q", "--limit", "50"]).is_ok());
        assert!(try_lower(&["references", "--guid", GUID, "--limit", "0"]).is_err());
        assert!(try_lower(&["references", "--guid", GUID, "--limit", "500"]).is_ok());
    }

    #[test]
    fn removed_http_and_token_commands_stay_rejected() {
        for arguments in [
            &["--base-url", "http://127.0.0.1:7777", "status"][..],
            &["--token", "obsolete", "status"][..],
            &["health"][..],
            &["bench"][..],
            &["reindex", "--full"][..],
        ] {
            assert!(
                Args::try_parse_from(
                    std::iter::once("unity-asset-search-cli").chain(arguments.iter().copied())
                )
                .is_err(),
                "removed command unexpectedly parsed: {arguments:?}"
            );
        }
    }

    #[test]
    fn changed_paths_are_canonicalized_before_protocol_validation() {
        let operation = lower_operation(&[
            "reindex",
            "admit",
            "--path",
            "Assets/Z.asset",
            "--path",
            "Assets/A.asset",
            "--path",
            "Assets/Z.asset",
        ]);
        let RequestOperation::ReindexAdmit(request) = operation else {
            panic!("expected reindex admission");
        };
        let encoded = serde_json::to_value(request.intent).expect("intent JSON");
        assert_eq!(
            encoded["scope"]["paths"],
            serde_json::json!(["Assets/A.asset", "Assets/Z.asset"])
        );
    }

    fn lower_operation(arguments: &[&str]) -> RequestOperation {
        match try_lower(arguments).expect("lower CLI command") {
            Action::Operation(operation) => operation,
            other => panic!("expected protocol operation, got {other:?}"),
        }
    }

    fn try_lower(arguments: &[&str]) -> Result<Action, crate::output::CliFailure> {
        let args = Args::try_parse_from(
            std::iter::once("unity-asset-search-cli").chain(arguments.iter().copied()),
        )
        .expect("parse CLI arguments");
        args.action()
    }
}
