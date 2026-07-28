use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::thread;

use unity_asset_search_index::{ApiError, ApiErrorCode, SearchResponse};

fn run_cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unity-asset-search-cli"))
        .args(arguments)
        .output()
        .expect("run search CLI fixture")
}

fn parse_stderr(output: &Output) -> ApiError {
    assert!(!output.status.success(), "CLI unexpectedly succeeded");
    assert!(
        output.stdout.is_empty(),
        "stdout must contain no error output"
    );
    serde_json::from_slice(&output.stderr).expect("stderr must contain one ApiError JSON envelope")
}

fn serve_json_once(status: &'static str, body: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
    let address = listener.local_addr().expect("read HTTP fixture address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept CLI request");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("read CLI request");
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write fixture headers");
        stream.write_all(&body).expect("write fixture response");
    });
    (format!("http://{address}"), server)
}

fn search_response_json(contract_version: u16) -> Vec<u8> {
    let digest = format!("blake3-v1:{}", "00".repeat(32));
    serde_json::to_vec(&serde_json::json!({
        "contract_version": contract_version,
        "generation": {
            "contract_version": 2,
            "generation": digest,
            "workspace": "workspace-v1:00000000000000000000000000000001",
            "actual_revision": digest,
            "desired_revision": digest,
            "stale": false
        },
        "query": "fixture",
        "took_ms": 1,
        "match_count": {
            "value": 0,
            "relation": "exact"
        },
        "returned_hits": 0,
        "request_limit_truncated": false,
        "fuzzy_work": {
            "consumed": 0,
            "limit": 10,
            "exhausted": false
        },
        "hits": [],
        "diagnostics": [],
        "fallback_used": false
    }))
    .expect("serialize search response fixture")
}

#[test]
fn daemon_api_error_is_emitted_as_its_original_json_envelope() {
    let body = br#"{"contract_version":2,"code":"invalid_request","message":"fixture rejected","retryable":false,"details":{"field":"q"}}"#;
    let (base_url, server) = serve_json_once("400 Bad Request", body.to_vec());
    let output = run_cli(&["--base-url", &base_url, "health"]);
    server.join().expect("finish HTTP fixture");

    let error = parse_stderr(&output);
    assert_eq!(error.code, ApiErrorCode::InvalidRequest);
    assert_eq!(error.message, "fixture rejected");
    assert_eq!(error.details.get("field"), Some(&"q".to_string()));
}

#[test]
fn successful_search_is_emitted_as_versioned_json() {
    let (base_url, server) = serve_json_once("200 OK", search_response_json(2));
    let output = run_cli(&["--base-url", &base_url, "search", "fixture"]);
    server.join().expect("finish HTTP fixture");

    assert!(
        output.status.success(),
        "search CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "successful stderr must be empty");
    let response: SearchResponse =
        serde_json::from_slice(&output.stdout).expect("stdout must contain one SearchResponse");
    assert_eq!(response.contract_version, 2);
    assert_eq!(response.generation.contract_version, 2);
    assert_eq!(response.query, "fixture");
    assert_eq!(response.returned_hits, 0);
}

#[test]
fn older_and_newer_search_contract_versions_are_structured_process_errors() {
    for contract_version in [1, 3] {
        let (base_url, server) = serve_json_once("200 OK", search_response_json(contract_version));
        let output = run_cli(&["--base-url", &base_url, "search", "fixture"]);
        server.join().expect("finish HTTP fixture");

        let error = parse_stderr(&output);
        assert_eq!(error.code, ApiErrorCode::Internal);
        assert_eq!(
            error.details.get("source"),
            Some(&"unity_asset_search_cli".to_string())
        );
        assert!(
            error
                .message
                .contains("validate response contract GET search"),
            "unexpected version error for contract {contract_version}: {}",
            error.message
        );
    }
}

#[test]
fn transport_failure_is_emitted_as_a_json_error_envelope() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unused local port");
    let address = listener.local_addr().expect("read reserved local port");
    drop(listener);

    let base_url = format!("http://{address}");
    let output = run_cli(&["--base-url", &base_url, "health"]);
    let error = parse_stderr(&output);

    assert_eq!(error.code, ApiErrorCode::Internal);
    assert_eq!(
        error.details.get("source"),
        Some(&"unity_asset_search_cli".to_string())
    );
}
