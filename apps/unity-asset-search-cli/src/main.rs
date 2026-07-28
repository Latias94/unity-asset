use std::fmt;
use std::io::{self, Write};
use std::mem::size_of;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::error::ErrorKind;
use clap::{Parser, Subcommand};
use reqwest::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use unity_asset_core::{
    AssetLoadBudget, AssetLoadLimits, ContractJsonLimits, ContractJsonResourceModel,
    read_contract_json_slice,
};
use unity_asset_search_index::{
    ApiError, ApiErrorCode, FilesystemReindexIntent, ReferenceRequest, ReferencesResponse,
    SearchResponse, StatusResponse, SuggestResponse,
};
use unity_asset_search_protocol::{
    HEALTH_ENDPOINT, HealthResponse, REFERENCES_ENDPOINT, REINDEX_ENDPOINT, ReindexResponse,
    SEARCH_ENDPOINT, STATUS_ENDPOINT, SUGGEST_ENDPOINT, ValidateContractVersion,
};

const JSON_PARSER_WORK_MULTIPLIER: u64 = 6;
const JSON_PARSER_FIXED_WORK_BYTES: u64 = 4 * 1024;
const DEFAULT_CONNECT_TIMEOUT_SECONDS: &str = "10";
const DEFAULT_RESPONSE_TIMEOUT_SECONDS: &str = "60";
const DEFAULT_REINDEX_RESPONSE_HEADER_TIMEOUT_SECONDS: &str = "7200";
const DEFAULT_BODY_IDLE_TIMEOUT_SECONDS: &str = "15";

type CliResult<T> = std::result::Result<T, CliError>;

/// The only error payload written by this binary.
///
/// API responses retain their daemon-provided envelope. Local failures are translated into the
/// same versioned contract at the process boundary, so stderr is always one JSON document.
#[derive(Debug)]
struct CliError {
    api_error: ApiError,
}

impl CliError {
    const LOCAL_SOURCE: &'static str = "unity_asset_search_cli";

    fn api(api_error: ApiError) -> Self {
        Self { api_error }
    }

    fn local(message: impl Into<String>, retryable: bool) -> Self {
        Self::api(
            ApiError::new(ApiErrorCode::Internal, message, retryable)
                .with_detail("source", Self::LOCAL_SOURCE),
        )
    }

    fn invalid_request(message: impl Into<String>) -> Self {
        Self::api(
            ApiError::new(ApiErrorCode::InvalidRequest, message, false)
                .with_detail("source", Self::LOCAL_SOURCE),
        )
    }

    fn api_error(&self) -> &ApiError {
        &self.api_error
    }
}

impl From<anyhow::Error> for CliError {
    fn from(error: anyhow::Error) -> Self {
        let retryable = error.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|error| error.is_connect() || error.is_timeout())
        });
        Self::local(format!("{error:#}"), retryable)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::local(error.to_string(), false)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.api_error.message.fmt(formatter)
    }
}

impl std::error::Error for CliError {}

fn serialize_error(error: &CliError) -> serde_json::Result<String> {
    serde_json::to_string(error.api_error())
}

fn write_error(error: &CliError) -> io::Result<()> {
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    let json = serialize_error(error).map_err(io::Error::other)?;
    writeln!(stderr, "{json}")
}

#[derive(Debug, Clone, Copy)]
struct HttpTimeouts {
    connect: Duration,
    response: Duration,
    reindex_response_headers: Duration,
    body_idle: Duration,
}

impl HttpTimeouts {
    fn from_args(args: &Args) -> Self {
        Self {
            connect: Duration::from_secs(args.connect_timeout_secs.get()),
            response: Duration::from_secs(args.response_timeout_secs.get()),
            reindex_response_headers: Duration::from_secs(
                args.reindex_response_header_timeout_secs.get(),
            ),
            body_idle: Duration::from_secs(args.body_idle_timeout_secs.get()),
        }
    }

    const fn standard(self) -> FetchTimeouts {
        FetchTimeouts {
            connect: self.connect,
            response_headers: self.response,
            response_body: self.response,
            body_idle: self.body_idle,
            body_deadline_includes_headers: true,
        }
    }

    const fn reindex(self) -> FetchTimeouts {
        FetchTimeouts {
            connect: self.connect,
            response_headers: self.reindex_response_headers,
            response_body: self.response,
            body_idle: self.body_idle,
            body_deadline_includes_headers: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FetchTimeouts {
    connect: Duration,
    response_headers: Duration,
    response_body: Duration,
    body_idle: Duration,
    body_deadline_includes_headers: bool,
}

struct HttpSession {
    client: Client,
    standard_timeouts: FetchTimeouts,
    reindex_timeouts: FetchTimeouts,
}

impl HttpSession {
    fn new(timeouts: HttpTimeouts) -> Result<Self> {
        Ok(Self {
            client: http_client(timeouts.connect)?,
            standard_timeouts: timeouts.standard(),
            reindex_timeouts: timeouts.reindex(),
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ResponseJsonProfile {
    contract: &'static str,
    max_encoded_bytes: usize,
    max_depth: u32,
    max_entries: u64,
    max_members: u64,
    materialization_fixed_bytes: u64,
    materialization_bytes_per_entry: u64,
}

impl ResponseJsonProfile {
    const fn limits(self) -> ContractJsonLimits {
        ContractJsonLimits::new(
            self.contract,
            self.max_encoded_bytes,
            self.max_depth,
            self.max_entries,
            self.max_members,
            ContractJsonResourceModel::new(
                JSON_PARSER_WORK_MULTIPLIER,
                JSON_PARSER_FIXED_WORK_BYTES,
                self.materialization_fixed_bytes,
                self.materialization_bytes_per_entry,
            ),
        )
    }

    fn budget<T>(self) -> Result<AssetLoadBudget> {
        let encoded_bytes = u64::try_from(self.max_encoded_bytes)
            .context("response JSON byte limit does not fit the load budget")?;
        let root_layout = u64::try_from(size_of::<T>())
            .context("response JSON root layout does not fit the load budget")?;
        let decoded_input_and_parser = encoded_bytes
            .checked_mul(JSON_PARSER_WORK_MULTIPLIER + 1)
            .context("response JSON parser budget overflow")?;
        let materialization = self
            .max_entries
            .checked_mul(self.materialization_bytes_per_entry)
            .and_then(|bytes| bytes.checked_add(self.materialization_fixed_bytes))
            .and_then(|bytes| bytes.checked_add(root_layout))
            .context("response JSON materialization budget overflow")?;
        // The streamed body remains alive while the contract reader retains its own bounded copy.
        let max_bytes = encoded_bytes
            .checked_add(JSON_PARSER_FIXED_WORK_BYTES)
            .and_then(|bytes| bytes.checked_add(decoded_input_and_parser))
            .and_then(|bytes| bytes.checked_add(materialization))
            .context("response JSON load budget overflow")?;

        AssetLoadBudget::new(AssetLoadLimits {
            max_entries: self.max_entries,
            max_bytes,
            max_depth: self.max_depth,
            max_members: self.max_members,
            ..AssetLoadLimits::default()
        })
        .with_context(|| format!("create {} load budget", self.contract))
    }
}

const API_ERROR_JSON: ResponseJsonProfile = ResponseJsonProfile {
    contract: "search-cli.api-error.response",
    max_encoded_bytes: 1024 * 1024,
    max_depth: 16,
    max_entries: 8 * 1024,
    max_members: 8 * 1024,
    materialization_fixed_bytes: 1024 * 1024,
    materialization_bytes_per_entry: 512,
};
const HEALTH_RESPONSE_JSON: ResponseJsonProfile = ResponseJsonProfile {
    contract: "search-cli.health.response",
    max_encoded_bytes: 64 * 1024,
    max_depth: 4,
    max_entries: 128,
    max_members: 64,
    materialization_fixed_bytes: 64 * 1024,
    materialization_bytes_per_entry: 256,
};
const SEARCH_RESPONSE_JSON: ResponseJsonProfile = ResponseJsonProfile {
    contract: "search-cli.search.response",
    // Search candidates are capped at 4 MiB by the daemon; allow for duplicated display fields,
    // highlights, diagnostics, and the response envelope.
    max_encoded_bytes: 32 * 1024 * 1024,
    max_depth: 24,
    max_entries: 256 * 1024,
    max_members: 256 * 1024,
    materialization_fixed_bytes: 32 * 1024 * 1024,
    materialization_bytes_per_entry: 512,
};
const REFERENCES_RESPONSE_JSON: ResponseJsonProfile = ResponseJsonProfile {
    contract: "search-cli.references.response",
    // The daemon caps serialized hits at 8 MiB and diagnostics at 256 KiB.
    max_encoded_bytes: 12 * 1024 * 1024,
    max_depth: 32,
    max_entries: 1024 * 1024,
    max_members: 1024 * 1024,
    materialization_fixed_bytes: 12 * 1024 * 1024,
    materialization_bytes_per_entry: 512,
};
const SUGGEST_RESPONSE_JSON: ResponseJsonProfile = ResponseJsonProfile {
    contract: "search-cli.suggest.response",
    // The daemon returns at most 50 suggestions and accepts a 4 KiB prefix.
    max_encoded_bytes: 4 * 1024 * 1024,
    max_depth: 8,
    max_entries: 4 * 1024,
    max_members: 4 * 1024,
    materialization_fixed_bytes: 4 * 1024 * 1024,
    materialization_bytes_per_entry: 256,
};
const STATUS_RESPONSE_JSON: ResponseJsonProfile = ResponseJsonProfile {
    contract: "search-cli.status.response",
    // Status retains project paths and configured scan roots.
    max_encoded_bytes: 4 * 1024 * 1024,
    max_depth: 16,
    max_entries: 32 * 1024,
    max_members: 32 * 1024,
    materialization_fixed_bytes: 4 * 1024 * 1024,
    materialization_bytes_per_entry: 256,
};
const REINDEX_RESPONSE_JSON: ResponseJsonProfile = ResponseJsonProfile {
    contract: "search-cli.reindex.response",
    // A waited response embeds status and may also retain publication warnings.
    max_encoded_bytes: 16 * 1024 * 1024,
    max_depth: 24,
    max_entries: 256 * 1024,
    max_members: 256 * 1024,
    materialization_fixed_bytes: 16 * 1024 * 1024,
    materialization_bytes_per_entry: 512,
};

#[derive(Debug, Parser)]
#[command(name = "unity-asset-search")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:9781")]
    base_url: String,

    /// Bearer token used only by reindex requests.
    #[arg(long)]
    token: Option<String>,

    /// Maximum time to establish one HTTP connection.
    #[arg(
        long,
        global = true,
        default_value = DEFAULT_CONNECT_TIMEOUT_SECONDS
    )]
    connect_timeout_secs: NonZeroU64,

    /// Maximum total time for an ordinary response, or for a reindex body after its headers.
    #[arg(
        long,
        global = true,
        default_value = DEFAULT_RESPONSE_TIMEOUT_SECONDS
    )]
    response_timeout_secs: NonZeroU64,

    /// Maximum time a waited reindex may take to return response headers.
    #[arg(
        long,
        global = true,
        default_value = DEFAULT_REINDEX_RESPONSE_HEADER_TIMEOUT_SECONDS
    )]
    reindex_response_header_timeout_secs: NonZeroU64,

    /// Maximum idle time between response body chunks.
    #[arg(
        long,
        global = true,
        default_value = DEFAULT_BODY_IDLE_TIMEOUT_SECONDS
    )]
    body_idle_timeout_secs: NonZeroU64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    Search {
        query: String,

        /// Filter by kind (shorthand for adding `type:<KIND>` to the query).
        ///
        /// Examples: `Prefab`, `Scene`, `Script`, `BundleContainer`.
        #[arg(long)]
        r#type: Option<String>,

        /// Filter by path prefix (shorthand for adding `in:"<PREFIX>"` to the query).
        ///
        /// Examples: `Assets/UI`, `Packages/com.company.product/`.
        #[arg(long)]
        in_path: Option<String>,

        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Health,
    References {
        guid: String,

        /// Signed Unity YAML fileID or binary pathID.
        #[arg(long, allow_negative_numbers = true)]
        file_id: Option<i64>,

        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Suggest {
        prefix: String,

        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Bench {
        #[arg(long)]
        query: Vec<String>,

        #[arg(long, default_value = "scripts/bench_queries.txt")]
        query_file: String,

        #[arg(long, default_value_t = 1)]
        warmup: usize,

        #[arg(long, default_value_t = 1)]
        repeat: usize,

        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Status,
    Reindex {
        /// Rebuild all indexed content.
        #[arg(long, conflicts_with_all = ["reconcile", "path"])]
        full: bool,

        /// Reconcile project state against the active generation (the default mode).
        #[arg(long, conflicts_with_all = ["full", "path"])]
        reconcile: bool,

        /// Reindex a changed project-relative path; may be repeated.
        #[arg(long, value_name = "PATH")]
        path: Vec<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    let result = match Args::try_parse() {
        Ok(args) => run(args).await,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(error) => Err(CliError::invalid_request(error.to_string())),
    };

    if let Err(error) = result {
        // Keep stderr unambiguous for automation even when its underlying handle is unavailable.
        let _ = write_error(&error);
        std::process::exit(1);
    }
}

async fn run(args: Args) -> CliResult<()> {
    let http = HttpSession::new(HttpTimeouts::from_args(&args))?;
    match args.cmd {
        Cmd::Search {
            query,
            r#type,
            in_path,
            limit,
        } => {
            let query = build_search_query(&query, r#type.as_deref(), in_path.as_deref());
            search(&http, &args.base_url, &query, limit).await?
        }
        Cmd::Health => health(&http, &args.base_url).await?,
        Cmd::References {
            guid,
            file_id,
            limit,
        } => references(&http, &args.base_url, &guid, file_id, limit).await?,
        Cmd::Suggest { prefix, limit } => suggest(&http, &args.base_url, &prefix, limit).await?,
        Cmd::Bench {
            query,
            query_file,
            warmup,
            repeat,
            limit,
        } => {
            bench(
                &http,
                &args.base_url,
                &query,
                &query_file,
                warmup,
                repeat,
                limit,
            )
            .await?
        }
        Cmd::Status => status(&http, &args.base_url).await?,
        Cmd::Reindex {
            full,
            reconcile,
            path,
        } => {
            reindex(
                &http,
                &args.base_url,
                args.token.as_deref(),
                reindex_intent(full, reconcile, &path)?,
            )
            .await?
        }
    }
    Ok(())
}

fn http_client(connect_timeout: Duration) -> Result<Client> {
    Client::builder()
        .connect_timeout(connect_timeout)
        .build()
        .with_context(|| format!("build HTTP client with {connect_timeout:?} connect timeout"))
}

fn build_search_query(raw: &str, kind: Option<&str>, in_path: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(kind) = kind.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("type:{kind}"));
    }
    if let Some(prefix) = in_path.map(str::trim).filter(|s| !s.is_empty()) {
        let quoted = if prefix.contains(' ') || prefix.contains('"') {
            prefix.replace('"', "\\\"")
        } else {
            prefix.to_string()
        };
        parts.push(format!("in:\"{quoted}\""));
    }
    let raw = raw.trim();
    if !raw.is_empty() {
        parts.push(raw.to_string());
    }
    parts.join(" ").trim().to_string()
}

async fn health(http: &HttpSession, base_url: &str) -> CliResult<()> {
    let response: HealthResponse = fetch_json(
        http.client.get(endpoint_url(base_url, HEALTH_ENDPOINT)),
        "GET health",
        HEALTH_RESPONSE_JSON,
        http.standard_timeouts,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn search(http: &HttpSession, base_url: &str, query: &str, limit: usize) -> CliResult<()> {
    let response: SearchResponse = fetch_json(
        http.client
            .get(endpoint_url(base_url, SEARCH_ENDPOINT))
            .query(&[("q", query), ("limit", &limit.to_string())]),
        "GET search",
        SEARCH_RESPONSE_JSON,
        http.standard_timeouts,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn status(http: &HttpSession, base_url: &str) -> CliResult<()> {
    let response: StatusResponse = fetch_json(
        http.client.get(endpoint_url(base_url, STATUS_ENDPOINT)),
        "GET status",
        STATUS_RESPONSE_JSON,
        http.standard_timeouts,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn suggest(http: &HttpSession, base_url: &str, prefix: &str, limit: usize) -> CliResult<()> {
    let response: SuggestResponse = fetch_json(
        http.client
            .get(endpoint_url(base_url, SUGGEST_ENDPOINT))
            .query(&[("prefix", prefix), ("limit", &limit.to_string())]),
        "GET suggest",
        SUGGEST_RESPONSE_JSON,
        http.standard_timeouts,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn reference_request(guid: &str, file_id: Option<i64>, limit: usize) -> ReferenceRequest {
    ReferenceRequest::incoming_guid(guid, file_id, limit)
}

async fn references(
    http: &HttpSession,
    base_url: &str,
    guid: &str,
    file_id: Option<i64>,
    limit: usize,
) -> CliResult<()> {
    let request = reference_request(guid, file_id, limit);
    let response: ReferencesResponse = fetch_json(
        http.client
            .post(endpoint_url(base_url, REFERENCES_ENDPOINT))
            .json(&request),
        "POST references",
        REFERENCES_RESPONSE_JSON,
        http.standard_timeouts,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn bench(
    http: &HttpSession,
    base_url: &str,
    inline_queries: &[String],
    query_file: &str,
    warmup: usize,
    repeat: usize,
    limit: usize,
) -> CliResult<()> {
    let mut queries = Vec::new();
    queries.extend(inline_queries.iter().cloned());
    queries.extend(load_queries_from_file(query_file).unwrap_or_default());
    queries.retain(|q| !q.trim().is_empty());

    if queries.is_empty() {
        return Err(CliError::invalid_request(
            "no queries provided (use --query or --query-file)",
        ));
    }

    for q in &queries {
        for _ in 0..warmup {
            let _ = search_once(http, base_url, q, limit).await?;
        }
    }

    let mut tooks = Vec::new();
    for q in &queries {
        for _ in 0..repeat {
            let took_ms = search_once(http, base_url, q, limit).await?;
            tooks.push(took_ms);
        }
    }
    tooks.sort();

    let p50 = percentile(&tooks, 0.50);
    let p95 = percentile(&tooks, 0.95);
    let max = tooks.last().copied().unwrap_or(0);

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "queries": queries.len(),
            "runs": tooks.len(),
            "p50_ms": p50,
            "p95_ms": p95,
            "max_ms": max,
        }))?
    );

    Ok(())
}

async fn search_once(
    http: &HttpSession,
    base_url: &str,
    query: &str,
    limit: usize,
) -> CliResult<u128> {
    let response: SearchResponse = fetch_json(
        http.client
            .get(endpoint_url(base_url, SEARCH_ENDPOINT))
            .query(&[("q", query), ("limit", &limit.to_string())]),
        &format!("GET search (q={query})"),
        SEARCH_RESPONSE_JSON,
        http.standard_timeouts,
    )
    .await?;
    Ok(response.took_ms)
}

fn load_queries_from_file(path: &str) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read queries: {path}"))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn reindex_intent(
    full: bool,
    reconcile: bool,
    paths: &[PathBuf],
) -> Result<FilesystemReindexIntent> {
    match (full, reconcile, paths) {
        (true, false, []) => Ok(FilesystemReindexIntent::full()),
        (false, _, []) => Ok(FilesystemReindexIntent::reconcile()),
        (false, false, paths) => Ok(FilesystemReindexIntent::changed_paths(paths.to_vec())),
        _ => anyhow::bail!("reindex modes --full, --reconcile, and --path are mutually exclusive"),
    }
}

async fn reindex(
    http: &HttpSession,
    base_url: &str,
    token: Option<&str>,
    intent: FilesystemReindexIntent,
) -> CliResult<()> {
    let mut req = http
        .client
        .post(endpoint_url(base_url, REINDEX_ENDPOINT))
        .json(&intent);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }

    let response: ReindexResponse = fetch_json(
        req,
        "POST reindex",
        REINDEX_RESPONSE_JSON,
        http.reindex_timeouts,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn endpoint_url(base_url: &str, endpoint: &str) -> String {
    format!("{}{endpoint}", base_url.trim_end_matches('/'))
}

async fn fetch_json<T>(
    req: RequestBuilder,
    ctx: &str,
    success_profile: ResponseJsonProfile,
    timeouts: FetchTimeouts,
) -> CliResult<T>
where
    T: DeserializeOwned + ValidateContractVersion,
{
    let request_started = tokio::time::Instant::now();
    let resp = send_request(req, ctx, timeouts).await?;
    let status = resp.status();

    if !status.is_success() {
        let mut budget = API_ERROR_JSON.budget::<ApiError>()?;
        let body = read_response_body(
            resp,
            ctx,
            API_ERROR_JSON,
            &mut budget,
            timeouts,
            request_started,
        )
        .await?;
        let error: ApiError = read_contract_json_slice(&body, &mut budget, API_ERROR_JSON.limits())
            .with_context(|| format!("parse API error {ctx} ({status})"))?;
        error
            .validate_contract_version()
            .with_context(|| format!("validate API error contract {ctx} ({status})"))?;
        return Err(CliError::api(error));
    }

    let mut budget = success_profile.budget::<T>()?;
    let body = read_response_body(
        resp,
        ctx,
        success_profile,
        &mut budget,
        timeouts,
        request_started,
    )
    .await?;
    let response: T = read_contract_json_slice(&body, &mut budget, success_profile.limits())
        .with_context(|| format!("parse json {ctx}"))?;
    response
        .validate_contract_version()
        .with_context(|| format!("validate response contract {ctx}"))?;
    Ok(response)
}

async fn send_request(
    req: RequestBuilder,
    ctx: &str,
    timeouts: FetchTimeouts,
) -> CliResult<Response> {
    match tokio::time::timeout(timeouts.response_headers, req.send()).await {
        Err(_) => Err(CliError::local(
            format!(
                "{ctx} timed out during request send/response headers after {:?}",
                timeouts.response_headers,
            ),
            true,
        )),
        Ok(Err(error)) if error.is_timeout() && error.is_connect() => Err(CliError::local(
            format!(
                "{ctx} timed out during connection establishment after {:?}: {error}",
                timeouts.connect,
            ),
            true,
        )),
        Ok(Err(error)) if error.is_timeout() => Err(CliError::local(
            format!(
                "{ctx} timed out during HTTP transport after {:?}: {error}",
                timeouts.response_headers,
            ),
            true,
        )),
        Ok(Err(error)) => Err(CliError::local(format!("request {ctx}: {error}"), false)),
        Ok(Ok(response)) => Ok(response),
    }
}

async fn read_response_body(
    response: Response,
    ctx: &str,
    profile: ResponseJsonProfile,
    budget: &mut AssetLoadBudget,
    timeouts: FetchTimeouts,
    request_started: tokio::time::Instant,
) -> Result<Vec<u8>> {
    let body_timeout = if timeouts.body_deadline_includes_headers {
        timeouts
            .response_body
            .checked_sub(request_started.elapsed())
            .unwrap_or(Duration::ZERO)
    } else {
        timeouts.response_body
    };
    match tokio::time::timeout(
        body_timeout,
        read_response_body_stream(response, ctx, profile, budget, timeouts.body_idle),
    )
    .await
    {
        Ok(result) => result,
        Err(_) if timeouts.body_deadline_includes_headers => anyhow::bail!(
            "{ctx} {contract} timed out during response headers/complete body after {:?}",
            timeouts.response_body,
            contract = profile.contract,
        ),
        Err(_) => anyhow::bail!(
            "{ctx} {contract} timed out during complete response body after {:?}",
            timeouts.response_body,
            contract = profile.contract,
        ),
    }
}

async fn read_response_body_stream(
    mut response: Response,
    ctx: &str,
    profile: ResponseJsonProfile,
    budget: &mut AssetLoadBudget,
    body_idle_timeout: Duration,
) -> Result<Vec<u8>> {
    let maximum = profile.max_encoded_bytes;
    let maximum_u64 =
        u64::try_from(maximum).context("response JSON byte limit does not fit u64")?;
    if let Some(declared) = response.content_length()
        && declared > maximum_u64
    {
        anyhow::bail!(
            "{ctx} {contract} Content-Length {declared} exceeds the {maximum}-byte response limit",
            contract = profile.contract,
        );
    }

    // Reserve the complete retained-body allowance before any caller-owned response buffer grows.
    budget
        .consume_bytes(maximum_u64)
        .with_context(|| format!("reserve {} response body budget", profile.contract))?;
    let mut body = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(body_idle_timeout, response.chunk()).await {
            Err(_) => anyhow::bail!(
                "{ctx} {contract} timed out while waiting for the next response body chunk after {body_idle_timeout:?}",
                contract = profile.contract,
            ),
            Ok(result) => result.with_context(|| format!("read body {ctx}"))?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let requested = body
            .len()
            .checked_add(chunk.len())
            .context("response JSON body length overflow")?;
        if requested > maximum {
            anyhow::bail!(
                "{ctx} {contract} body exceeds the {maximum}-byte response limit while streaming",
                contract = profile.contract,
            );
        }
        reserve_response_capacity(&mut body, chunk.len(), maximum, profile.contract)?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn reserve_response_capacity(
    body: &mut Vec<u8>,
    additional: usize,
    maximum: usize,
    contract: &'static str,
) -> Result<()> {
    let required = body
        .len()
        .checked_add(additional)
        .context("response JSON capacity overflow")?;
    if required <= body.capacity() {
        return Ok(());
    }
    let target = required
        .checked_next_power_of_two()
        .unwrap_or(maximum)
        .min(maximum);
    let reserve = target
        .checked_sub(body.len())
        .context("response JSON reserve underflow")?;
    body.try_reserve_exact(reserve)
        .with_context(|| format!("reserve {target} bytes for {contract}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::path::PathBuf;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use clap::Parser;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use unity_asset_search_index::{ApiError, ApiErrorCode};
    use unity_asset_search_protocol::HealthResponse;

    use super::{
        API_ERROR_JSON, Args, CliError, Cmd, FetchTimeouts, HEALTH_RESPONSE_JSON, HttpTimeouts,
        fetch_json, http_client, percentile, reference_request, reindex_intent, serialize_error,
    };

    const VALID_HEALTH_JSON: &[u8] = br#"{"contract_version":3,"ok":true,"version":"fixture"}"#;
    const TEST_FETCH_TIMEOUTS: FetchTimeouts = FetchTimeouts {
        connect: Duration::from_millis(500),
        response_headers: Duration::from_millis(100),
        response_body: Duration::from_millis(500),
        body_idle: Duration::from_millis(100),
        body_deadline_includes_headers: true,
    };
    const TEST_CASE_TIMEOUT: Duration = Duration::from_secs(2);

    enum FixtureResponse {
        Complete(Vec<u8>),
        StallAfter(Vec<u8>),
    }

    struct FixtureServer {
        task: Option<JoinHandle<Result<()>>>,
    }

    impl FixtureServer {
        fn new(task: JoinHandle<Result<()>>) -> Self {
            Self { task: Some(task) }
        }

        async fn stop(mut self) -> Result<()> {
            let task = self
                .task
                .take()
                .context("fixture server task was already consumed")?;
            task.abort();
            match task.await {
                Ok(result) => result,
                Err(error) if error.is_cancelled() => Ok(()),
                Err(error) => {
                    Err(anyhow::Error::new(error)).context("fixture server task panicked")
                }
            }
        }
    }

    impl Drop for FixtureServer {
        fn drop(&mut self) {
            if let Some(task) = &self.task {
                task.abort();
            }
        }
    }

    fn fixed_http_response(status: &str, declared: usize, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn chunked_http_response(status: &str, chunks: &[Vec<u8>], terminated: bool) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        for chunk in chunks {
            response.extend_from_slice(format!("{:X}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        if terminated {
            response.extend_from_slice(b"0\r\n\r\n");
        }
        response
    }

    async fn fetch_health_from_fixture(raw_response: Vec<u8>) -> super::CliResult<HealthResponse> {
        fetch_health_from_fixture_response(FixtureResponse::Complete(raw_response)).await
    }

    async fn fetch_health_from_fixture_response(
        fixture_response: FixtureResponse,
    ) -> super::CliResult<HealthResponse> {
        fetch_health_from_fixture_response_with_timeouts(fixture_response, TEST_FETCH_TIMEOUTS)
            .await
    }

    async fn fetch_health_from_fixture_response_with_timeouts(
        fixture_response: FixtureResponse,
        timeouts: FetchTimeouts,
    ) -> super::CliResult<HealthResponse> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind local HTTP fixture")?;
        let address = listener.local_addr().context("read fixture address")?;
        let server = FixtureServer::new(tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.context("accept fixture request")?;
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream
                    .read(&mut chunk)
                    .await
                    .context("read fixture request")?;
                if read == 0 {
                    anyhow::bail!("fixture client closed before sending complete headers");
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if request.len() > 64 * 1024 {
                    anyhow::bail!("fixture request headers exceeded 64 KiB");
                }
            }
            let (raw_response, stall_after_write) = match fixture_response {
                FixtureResponse::Complete(response) => (response, false),
                FixtureResponse::StallAfter(response) => (response, true),
            };
            stream
                .write_all(&raw_response)
                .await
                .context("write fixture response")?;
            if stall_after_write {
                future::pending::<()>().await;
            }
            let _ = stream.shutdown().await;
            Ok::<(), anyhow::Error>(())
        }));

        let client = http_client(timeouts.connect)?;
        let result = tokio::time::timeout(
            TEST_CASE_TIMEOUT,
            fetch_json(
                client.get(format!("http://{address}/")),
                "GET fixture health",
                HEALTH_RESPONSE_JSON,
                timeouts,
            ),
        )
        .await;
        server.stop().await?;
        result.with_context(|| {
            format!("fixture client exceeded the {TEST_CASE_TIMEOUT:?} test deadline")
        })?
    }

    #[tokio::test]
    async fn accepts_fragmented_chunked_response() -> Result<()> {
        let chunks = VALID_HEALTH_JSON
            .iter()
            .map(|byte| vec![*byte])
            .collect::<Vec<_>>();
        let response =
            fetch_health_from_fixture(chunked_http_response("200 OK", &chunks, true)).await?;

        assert!(response.ok);
        assert_eq!(response.version, "fixture");
        Ok(())
    }

    #[tokio::test]
    async fn response_header_timeout_rejects_a_stalled_server() -> Result<()> {
        let partial_headers = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n".to_vec();
        let error =
            fetch_health_from_fixture_response(FixtureResponse::StallAfter(partial_headers))
                .await
                .expect_err("stalled response headers must time out");
        let message = format!("{error:#}");

        assert!(message.contains("GET fixture health"), "{message}");
        assert!(
            message.contains("request send/response headers"),
            "{message}"
        );
        assert!(
            message.contains(&format!("{:?}", TEST_FETCH_TIMEOUTS.response_headers)),
            "{message}"
        );
        let envelope: ApiError = serde_json::from_str(&serialize_error(&error)?)?;
        assert_eq!(envelope.code, ApiErrorCode::Internal);
        assert!(envelope.retryable);
        Ok(())
    }

    #[tokio::test]
    async fn body_idle_timeout_rejects_a_stalled_chunk_stream() -> Result<()> {
        let partial_body = chunked_http_response("200 OK", &[b"abc".to_vec()], false);
        let error = fetch_health_from_fixture_response(FixtureResponse::StallAfter(partial_body))
            .await
            .expect_err("a stalled response body must time out");
        let message = format!("{error:#}");

        assert!(message.contains("GET fixture health"), "{message}");
        assert!(message.contains("next response body chunk"), "{message}");
        assert!(
            message.contains(&format!("{:?}", TEST_FETCH_TIMEOUTS.body_idle)),
            "{message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn total_response_timeout_bounds_a_stalled_body() -> Result<()> {
        let timeouts = FetchTimeouts {
            connect: Duration::from_secs(1),
            response_headers: Duration::from_secs(1),
            response_body: Duration::from_millis(200),
            body_idle: Duration::from_secs(1),
            body_deadline_includes_headers: true,
        };
        let partial_body = chunked_http_response("200 OK", &[b"abc".to_vec()], false);
        let error = fetch_health_from_fixture_response_with_timeouts(
            FixtureResponse::StallAfter(partial_body),
            timeouts,
        )
        .await
        .expect_err("the complete ordinary response must have a total deadline");
        let message = format!("{error:#}");

        assert!(message.contains("GET fixture health"), "{message}");
        assert!(
            message.contains("response headers/complete body"),
            "{message}"
        );
        assert!(
            message.contains(&format!("{:?}", timeouts.response_body)),
            "{message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_oversized_chunked_response() -> Result<()> {
        let first = vec![b'x'; HEALTH_RESPONSE_JSON.max_encoded_bytes / 2];
        let second = vec![b'x'; HEALTH_RESPONSE_JSON.max_encoded_bytes - first.len() + 1];
        let error =
            fetch_health_from_fixture(chunked_http_response("200 OK", &[first, second], true))
                .await
                .expect_err("an oversized chunked body must be rejected");
        let message = format!("{error:#}");

        assert!(message.contains("body exceeds"), "{message}");
        assert!(
            message.contains(&HEALTH_RESPONSE_JSON.max_encoded_bytes.to_string()),
            "{message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_forged_large_content_length_before_body_read() -> Result<()> {
        let declared = HEALTH_RESPONSE_JSON.max_encoded_bytes + 1;
        let error =
            fetch_health_from_fixture(fixed_http_response("200 OK", declared, VALID_HEALTH_JSON))
                .await
                .expect_err("an oversized Content-Length must be rejected");
        let message = format!("{error:#}");

        assert!(message.contains("Content-Length"), "{message}");
        assert!(message.contains(&declared.to_string()), "{message}");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_forged_small_content_length() -> Result<()> {
        let error = fetch_health_from_fixture(fixed_http_response("200 OK", 2, VALID_HEALTH_JSON))
            .await
            .expect_err("a body truncated by its Content-Length must not deserialize");
        let message = format!("{error:#}");

        assert!(
            message.contains("parse json GET fixture health"),
            "{message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn api_errors_use_an_independent_raw_body_limit() -> Result<()> {
        let declared = API_ERROR_JSON.max_encoded_bytes + 1;
        let error =
            fetch_health_from_fixture(fixed_http_response("400 Bad Request", declared, b"{}"))
                .await
                .expect_err("an oversized API error declaration must be rejected");
        let message = format!("{error:#}");

        assert!(message.contains(API_ERROR_JSON.contract), "{message}");
        assert!(message.contains(&declared.to_string()), "{message}");
        Ok(())
    }

    #[tokio::test]
    async fn preserves_typed_api_error_output() -> Result<()> {
        let body = br#"{"contract_version":2,"code":"invalid_request","message":"fixture rejected","retryable":false,"details":{"field":"q"}}"#;
        let error =
            fetch_health_from_fixture(fixed_http_response("400 Bad Request", body.len(), body))
                .await
                .expect_err("a typed API error must remain an application error");
        let parsed: ApiError = serde_json::from_str(&serialize_error(&error)?)?;

        assert_eq!(parsed, *error.api_error());
        assert_eq!(parsed.code, ApiErrorCode::InvalidRequest);
        assert_eq!(parsed.message, "fixture rejected");
        assert_eq!(parsed.details.get("field"), Some(&"q".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn json_contract_failures_serialize_as_local_api_errors() -> Result<()> {
        let body = br#"{"contract_version":3,"ok":true,"version":false}"#;
        let error = fetch_health_from_fixture(fixed_http_response("200 OK", body.len(), body))
            .await
            .expect_err("invalid health JSON must fail contract decoding");
        let parsed: ApiError = serde_json::from_str(&serialize_error(&error)?)?;

        assert_eq!(parsed.code, ApiErrorCode::Internal);
        assert_eq!(
            parsed.details.get("source"),
            Some(&CliError::LOCAL_SOURCE.to_string())
        );
        Ok(())
    }

    #[test]
    fn local_errors_serialize_as_versioned_api_error_envelopes() -> Result<()> {
        let error = CliError::local("fixture transport failure", true);
        let parsed: ApiError = serde_json::from_str(&serialize_error(&error)?)?;

        assert_eq!(parsed, *error.api_error());
        assert_eq!(parsed.code, ApiErrorCode::Internal);
        assert!(parsed.retryable);
        assert_eq!(
            parsed.details.get("source"),
            Some(&CliError::LOCAL_SOURCE.to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_response_json_beyond_depth_limit() -> Result<()> {
        let depth = usize::try_from(HEALTH_RESPONSE_JSON.max_depth + 1)
            .context("fixture depth does not fit usize")?;
        let body = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        let error =
            fetch_health_from_fixture(fixed_http_response("200 OK", body.len(), body.as_bytes()))
                .await
                .expect_err("deep JSON must be rejected before typed materialization");
        let message = format!("{error:#}");

        assert!(message.contains("depth limit"), "{message}");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_response_json_beyond_member_limit() -> Result<()> {
        let mut body = String::from(r#"{"contract_version":3,"ok":true,"version":"fixture""#);
        for index in 0..HEALTH_RESPONSE_JSON.max_members {
            body.push_str(&format!(r#","extra_{index}":0"#));
        }
        body.push('}');
        let error =
            fetch_health_from_fixture(fixed_http_response("200 OK", body.len(), body.as_bytes()))
                .await
                .expect_err("wide JSON must be rejected before typed materialization");
        let message = format!("{error:#}");

        assert!(message.contains("members limit"), "{message}");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_trailing_json_document() -> Result<()> {
        let mut body = VALID_HEALTH_JSON.to_vec();
        body.extend_from_slice(b"\nnull");
        let error = fetch_health_from_fixture(fixed_http_response("200 OK", body.len(), &body))
            .await
            .expect_err("a trailing JSON document must be rejected");
        let message = format!("{error:#}");

        assert!(message.contains("trailing characters"), "{message}");
        Ok(())
    }

    #[tokio::test]
    async fn reports_truncated_chunked_transport() -> Result<()> {
        let response = chunked_http_response("200 OK", &[b"abc".to_vec()], false);
        let error = fetch_health_from_fixture(response)
            .await
            .expect_err("a truncated chunk stream must be rejected");
        let message = format!("{error:#}");

        assert!(
            message.contains("read body GET fixture health"),
            "{message}"
        );
        Ok(())
    }

    #[test]
    fn percentile_handles_empty() {
        assert_eq!(percentile(&[], 0.50), 0);
    }

    #[test]
    fn percentile_picks_endpoints() {
        let sorted = [10u128, 20, 30, 40];
        assert_eq!(percentile(&sorted, 0.0), 10);
        assert_eq!(percentile(&sorted, 1.0), 40);
    }

    #[test]
    fn signed_reference_request_matches_wire_contract() -> Result<()> {
        let request = reference_request("deadbeefdeadbeefdeadbeefdeadbeef", Some(-11_500_000), 50);

        assert_eq!(
            serde_json::to_value(request)?,
            json!({
                "contract_version": 2,
                "direction": "incoming",
                "selector": {
                    "kind": "guid",
                    "guid": "deadbeefdeadbeefdeadbeefdeadbeef",
                    "file_id": -11_500_000
                },
                "limit": 50
            })
        );
        Ok(())
    }

    #[test]
    fn reindex_modes_match_exact_wire_contract() -> Result<()> {
        assert_eq!(
            serde_json::to_value(reindex_intent(true, false, &[])?)?,
            json!({
                "contract_version": 2,
                "scope": { "kind": "full" }
            })
        );
        assert_eq!(
            serde_json::to_value(reindex_intent(false, false, &[])?)?,
            json!({
                "contract_version": 2,
                "scope": { "kind": "reconcile" }
            })
        );
        assert_eq!(
            serde_json::to_value(reindex_intent(false, true, &[])?)?,
            json!({
                "contract_version": 2,
                "scope": { "kind": "reconcile" }
            })
        );

        let paths = [
            PathBuf::from("Assets/UI/Menu.prefab"),
            PathBuf::from("Packages/com.example/Runtime.asset"),
        ];
        assert_eq!(
            serde_json::to_value(reindex_intent(false, false, &paths)?)?,
            json!({
                "contract_version": 2,
                "scope": {
                    "kind": "changed_paths",
                    "paths": [
                        "Assets/UI/Menu.prefab",
                        "Packages/com.example/Runtime.asset"
                    ]
                }
            })
        );
        Ok(())
    }

    #[test]
    fn clap_rejects_zero_http_timeouts() {
        for flag in [
            "--connect-timeout-secs",
            "--response-timeout-secs",
            "--reindex-response-header-timeout-secs",
            "--body-idle-timeout-secs",
        ] {
            assert!(
                Args::try_parse_from(["unity-asset-search", flag, "0", "health"]).is_err(),
                "{flag} accepted a zero-second timeout"
            );
        }
    }

    #[test]
    fn reindex_only_extends_the_response_header_deadline() -> Result<()> {
        let args = Args::try_parse_from([
            "unity-asset-search",
            "--connect-timeout-secs",
            "2",
            "--response-timeout-secs",
            "3",
            "--reindex-response-header-timeout-secs",
            "4",
            "--body-idle-timeout-secs",
            "1",
            "health",
        ])?;
        let configured = HttpTimeouts::from_args(&args);
        let standard = configured.standard();
        let reindex = configured.reindex();

        assert_eq!(standard.connect, Duration::from_secs(2));
        assert_eq!(standard.response_headers, Duration::from_secs(3));
        assert_eq!(standard.response_body, Duration::from_secs(3));
        assert_eq!(standard.body_idle, Duration::from_secs(1));
        assert!(standard.body_deadline_includes_headers);

        assert_eq!(reindex.connect, standard.connect);
        assert_eq!(reindex.response_headers, Duration::from_secs(4));
        assert_eq!(reindex.response_body, standard.response_body);
        assert_eq!(reindex.body_idle, standard.body_idle);
        assert!(!reindex.body_deadline_includes_headers);
        Ok(())
    }

    #[test]
    fn clap_rejects_conflicting_reindex_modes() {
        assert!(
            Args::try_parse_from(["unity-asset-search", "reindex", "--full", "--reconcile"])
                .is_err()
        );
        assert!(
            Args::try_parse_from([
                "unity-asset-search",
                "reindex",
                "--full",
                "--path",
                "Assets/UI/Menu.prefab"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "unity-asset-search",
                "reindex",
                "--reconcile",
                "--path",
                "Assets/UI/Menu.prefab"
            ])
            .is_err()
        );
    }

    #[test]
    fn clap_accepts_negative_reference_file_id() -> Result<()> {
        let args = Args::try_parse_from([
            "unity-asset-search",
            "references",
            "deadbeefdeadbeefdeadbeefdeadbeef",
            "--file-id",
            "-11500000",
        ])?;

        let Cmd::References { file_id, .. } = args.cmd else {
            anyhow::bail!("expected references command");
        };
        assert_eq!(file_id, Some(-11_500_000));
        Ok(())
    }
}
