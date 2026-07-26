use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::RequestBuilder;
use serde::de::DeserializeOwned;
use unity_asset_search_index::{
    ApiError, ReferenceRequest, ReferencesResponse, ReindexIntent, SearchResponse, StatusResponse,
    SuggestResponse,
};
use unity_asset_search_protocol::{
    HEALTH_ENDPOINT, HealthResponse, REFERENCES_ENDPOINT, REINDEX_ENDPOINT, ReindexResponse,
    SEARCH_ENDPOINT, STATUS_ENDPOINT, SUGGEST_ENDPOINT, ValidateContractVersion,
};

#[derive(Debug, Parser)]
#[command(name = "unity-asset-search")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:9781")]
    base_url: String,

    /// Bearer token used only by reindex requests.
    #[arg(long)]
    token: Option<String>,

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
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Search {
            query,
            r#type,
            in_path,
            limit,
        } => {
            let query = build_search_query(&query, r#type.as_deref(), in_path.as_deref());
            search(&args.base_url, &query, limit).await?
        }
        Cmd::Health => health(&args.base_url).await?,
        Cmd::References {
            guid,
            file_id,
            limit,
        } => references(&args.base_url, &guid, file_id, limit).await?,
        Cmd::Suggest { prefix, limit } => suggest(&args.base_url, &prefix, limit).await?,
        Cmd::Bench {
            query,
            query_file,
            warmup,
            repeat,
            limit,
        } => bench(&args.base_url, &query, &query_file, warmup, repeat, limit).await?,
        Cmd::Status => status(&args.base_url).await?,
        Cmd::Reindex {
            full,
            reconcile,
            path,
        } => {
            reindex(
                &args.base_url,
                args.token.as_deref(),
                reindex_intent(full, reconcile, &path)?,
            )
            .await?
        }
    }
    Ok(())
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

async fn health(base_url: &str) -> Result<()> {
    let response: HealthResponse = fetch_json(
        reqwest::Client::new().get(endpoint_url(base_url, HEALTH_ENDPOINT)),
        "GET health",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn search(base_url: &str, query: &str, limit: usize) -> Result<()> {
    let response: SearchResponse = fetch_json(
        reqwest::Client::new()
            .get(endpoint_url(base_url, SEARCH_ENDPOINT))
            .query(&[("q", query), ("limit", &limit.to_string())]),
        "GET search",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn status(base_url: &str) -> Result<()> {
    let response: StatusResponse = fetch_json(
        reqwest::Client::new().get(endpoint_url(base_url, STATUS_ENDPOINT)),
        "GET status",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn suggest(base_url: &str, prefix: &str, limit: usize) -> Result<()> {
    let response: SuggestResponse = fetch_json(
        reqwest::Client::new()
            .get(endpoint_url(base_url, SUGGEST_ENDPOINT))
            .query(&[("prefix", prefix), ("limit", &limit.to_string())]),
        "GET suggest",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn reference_request(guid: &str, file_id: Option<i64>, limit: usize) -> ReferenceRequest {
    ReferenceRequest::incoming_guid(guid, file_id, limit)
}

async fn references(base_url: &str, guid: &str, file_id: Option<i64>, limit: usize) -> Result<()> {
    let request = reference_request(guid, file_id, limit);
    let response: ReferencesResponse = fetch_json(
        reqwest::Client::new()
            .post(endpoint_url(base_url, REFERENCES_ENDPOINT))
            .json(&request),
        "POST references",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn bench(
    base_url: &str,
    inline_queries: &[String],
    query_file: &str,
    warmup: usize,
    repeat: usize,
    limit: usize,
) -> Result<()> {
    let mut queries = Vec::new();
    queries.extend(inline_queries.iter().cloned());
    queries.extend(load_queries_from_file(query_file).unwrap_or_default());
    queries.retain(|q| !q.trim().is_empty());

    if queries.is_empty() {
        anyhow::bail!("no queries provided (use --query or --query-file)");
    }

    let client = reqwest::Client::new();
    for q in &queries {
        for _ in 0..warmup {
            let _ = search_once(&client, base_url, q, limit).await?;
        }
    }

    let mut tooks = Vec::new();
    for q in &queries {
        for _ in 0..repeat {
            let took_ms = search_once(&client, base_url, q, limit).await?;
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
    client: &reqwest::Client,
    base_url: &str,
    query: &str,
    limit: usize,
) -> Result<u128> {
    let response: SearchResponse = fetch_json(
        client
            .get(endpoint_url(base_url, SEARCH_ENDPOINT))
            .query(&[("q", query), ("limit", &limit.to_string())]),
        &format!("GET search (q={query})"),
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

fn reindex_intent(full: bool, reconcile: bool, paths: &[PathBuf]) -> Result<ReindexIntent> {
    match (full, reconcile, paths) {
        (true, false, []) => Ok(ReindexIntent::full()),
        (false, _, []) => Ok(ReindexIntent::reconcile()),
        (false, false, paths) => Ok(ReindexIntent::changed_paths(paths.to_vec())),
        _ => anyhow::bail!("reindex modes --full, --reconcile, and --path are mutually exclusive"),
    }
}

async fn reindex(base_url: &str, token: Option<&str>, intent: ReindexIntent) -> Result<()> {
    let mut req = reqwest::Client::new()
        .post(endpoint_url(base_url, REINDEX_ENDPOINT))
        .json(&intent);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }

    let response: ReindexResponse = fetch_json(req, "POST reindex").await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn endpoint_url(base_url: &str, endpoint: &str) -> String {
    format!("{}{endpoint}", base_url.trim_end_matches('/'))
}

async fn fetch_json<T>(req: RequestBuilder, ctx: &str) -> Result<T>
where
    T: DeserializeOwned + ValidateContractVersion,
{
    let resp = req.send().await.with_context(|| format!("request {ctx}"))?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .with_context(|| format!("read body {ctx}"))?;

    if !status.is_success() {
        let error: ApiError = serde_json::from_slice(&body)
            .with_context(|| format!("parse API error {ctx} ({status})"))?;
        error
            .validate_contract_version()
            .with_context(|| format!("validate API error contract {ctx} ({status})"))?;
        let error_json = serde_json::to_string(&error).context("serialize API error")?;
        anyhow::bail!("{ctx} failed: {status}: {error_json}");
    }

    let response: T = serde_json::from_slice(&body).with_context(|| format!("parse json {ctx}"))?;
    response
        .validate_contract_version()
        .with_context(|| format!("validate response contract {ctx}"))?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use clap::Parser;
    use serde_json::json;

    use super::{Args, Cmd, percentile, reference_request, reindex_intent};

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
                "contract_version": 1,
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
                "contract_version": 1,
                "scope": { "kind": "full" }
            })
        );
        assert_eq!(
            serde_json::to_value(reindex_intent(false, false, &[])?)?,
            json!({
                "contract_version": 1,
                "scope": { "kind": "reconcile" }
            })
        );
        assert_eq!(
            serde_json::to_value(reindex_intent(false, true, &[])?)?,
            json!({
                "contract_version": 1,
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
                "contract_version": 1,
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
