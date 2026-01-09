use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::RequestBuilder;

#[derive(Debug, Parser)]
#[command(name = "unity-asset-search")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:9781")]
    base_url: String,

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

        #[arg(long)]
        file_id: Option<u64>,

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
        #[arg(long)]
        full: bool,

        #[arg(long, value_name = "PATH")]
        path: Vec<String>,
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
        Cmd::Reindex { full, path } => {
            reindex(&args.base_url, args.token.as_deref(), full, &path).await?
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
    let url = format!("{base_url}/v1/health");
    let json = fetch_json(reqwest::Client::new().get(url), "GET /v1/health").await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn search(base_url: &str, query: &str, limit: usize) -> Result<()> {
    let url = format!("{base_url}/v1/search");
    let json = fetch_json(
        reqwest::Client::new()
            .get(url)
            .query(&[("q", query), ("limit", &limit.to_string())]),
        "GET /v1/search",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn status(base_url: &str) -> Result<()> {
    let url = format!("{base_url}/v1/status");
    let json = fetch_json(reqwest::Client::new().get(url), "GET /v1/status").await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn suggest(base_url: &str, prefix: &str, limit: usize) -> Result<()> {
    let url = format!("{base_url}/v1/suggest");
    let json = fetch_json(
        reqwest::Client::new()
            .get(url)
            .query(&[("prefix", prefix), ("limit", &limit.to_string())]),
        "GET /v1/suggest",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn references(base_url: &str, guid: &str, file_id: Option<u64>, limit: usize) -> Result<()> {
    let url = format!("{base_url}/v1/references");
    let mut params: Vec<(String, String)> = vec![
        ("guid".to_string(), guid.to_string()),
        ("limit".to_string(), limit.to_string()),
    ];
    if let Some(file_id) = file_id {
        params.push(("file_id".to_string(), file_id.to_string()));
    }
    let json = fetch_json(
        reqwest::Client::new().get(url).query(&params),
        "GET /v1/references",
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
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
    let url = format!("{base_url}/v1/search");
    let json = fetch_json(
        client
            .get(url)
            .query(&[("q", query), ("limit", &limit.to_string())]),
        &format!("GET /v1/search (q={query})"),
    )
    .await?;
    let took_ms = json
        .get("took_ms")
        .and_then(|v| v.as_u64())
        .map(|v| v as u128)
        .unwrap_or(0);
    Ok(took_ms)
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

async fn reindex(base_url: &str, token: Option<&str>, full: bool, paths: &[String]) -> Result<()> {
    let mut url = reqwest::Url::parse(&format!("{base_url}/v1/reindex"))?;
    {
        let mut qp = url.query_pairs_mut();
        if full {
            qp.append_pair("full", "true");
        }
        for p in paths {
            qp.append_pair("path", p);
        }
    }

    let mut req = reqwest::Client::new().post(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }

    let json = fetch_json(req, "POST /v1/reindex").await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

async fn fetch_json(req: RequestBuilder, ctx: &str) -> Result<serde_json::Value> {
    let resp = req.send().await.with_context(|| format!("request {ctx}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .with_context(|| format!("read body {ctx}"))?;

    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&body);
    if !status.is_success() {
        if let Ok(json) = &parsed {
            if let Some(msg) = json.get("error").and_then(|v| v.as_str()) {
                anyhow::bail!("{ctx} failed: {status}: {msg}");
            }
        }
        anyhow::bail!("{ctx} failed: {status}: {body}");
    }

    parsed.with_context(|| format!("parse json {ctx}"))
}

#[cfg(test)]
mod tests {
    use super::percentile;

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
}
