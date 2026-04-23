//! Phase-1 wrap-up (Task C): MCP "11 tools, no schema drift" regression.
//!
//! Locks two invariants of the v0.3.0 frozen MCP surface
//! (`docs/v0.3.0-frozen-api.md` §4):
//!
//!   1. **Tool set is exactly the frozen 11.** Renaming or accidentally
//!      dropping a tool would silently break MCP clients (Cursor /
//!      Claude Desktop / agent wrappers) that pin against these names.
//!
//!   2. **Per-tool input schema does not drift.** Every tool's
//!      `input_schema` (the JSON Schema object surfaced via
//!      `tools/list`) is snapshotted under `tests/snapshots/<tool>.json`.
//!      A diff in the snapshot is an intentional API change and must be
//!      reviewed in PR. Run with `AEROSYNC_UPDATE_SNAPSHOTS=1` to
//!      regenerate after a deliberate schema change.
//!
//!   3. **Each tool is callable.** A minimal valid argument set is sent
//!      to each tool; the response must be a JSON object containing
//!      either `success: true` (for tools that succeed without external
//!      state) or a stable, documented error message (for the few that
//!      legitimately need external state — e.g. `get_transfer_status`
//!      against a random uuid returns `Receipt/Task not found`).
//!
//! Companion to the existing E2E coverage in
//! `tests/mcp_integration.rs::e2e::test_e2e_lists_all_8_tools`, which
//! locks the count + names but not the schemas or per-tool callability.

use aerosync_mcp::server::AeroSyncMcpServer;
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use tempfile::tempdir;

#[derive(Debug, Clone, Default)]
struct DummyClient;
impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

/// Frozen tool set per `docs/v0.3.0-frozen-api.md` §4. The order is
/// the documented one — kept here so a drift ALSO fails an exact
/// vec equality assertion below, not just a set-membership check.
const FROZEN_TOOLS: &[&str] = &[
    "send_file",
    "send_directory",
    "start_receiver",
    "request_file",
    "stop_receiver",
    "get_receiver_status",
    "list_history",
    "discover_receivers",
    "get_transfer_status",
    "wait_receipt",
    "cancel_receipt",
];

fn snapshots_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
}

/// Stand up the MCP server over an in-memory duplex stream and return
/// a connected client + the server task handle.
async fn spawn_pair(
    server: AeroSyncMcpServer,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, DummyClient>,
    tokio::task::JoinHandle<()>,
) {
    let (server_t, client_t) = tokio::io::duplex(8192);
    let server_handle = tokio::spawn(async move {
        if let Ok(running) = server.serve(server_t).await {
            let _ = running.waiting().await;
        }
    });
    let client = DummyClient
        .serve(client_t)
        .await
        .expect("client should connect");
    (client, server_handle)
}

/// Pretty-print a JSON value with sorted keys + 2-space indent so
/// snapshot diffs are minimal and stable across `serde_json` upgrades.
fn canonical_pretty(v: &Value) -> String {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    sort_keys(v.clone()).serialize(&mut ser).expect("serialize");
    let mut s = String::from_utf8(buf).expect("utf8");
    s.push('\n');
    s
}

/// Recursively rebuild a JSON value with object keys sorted, so we
/// don't depend on `schemars` happening to emit fields in a
/// particular order.
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, Value> =
                std::collections::BTreeMap::new();
            for (k, val) in map {
                sorted.insert(k, sort_keys(val));
            }
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

use serde::Serialize;

// ── 1+2. List tools, check exact set, snapshot every input schema ────────

#[tokio::test]
async fn tool_set_and_schemas_match_golden_snapshot() {
    let dir = tempdir().unwrap();
    let server = AeroSyncMcpServer::new().with_aerosync_dir(dir.path().to_path_buf());
    let (client, _h) = spawn_pair(server).await;

    let listed = client
        .list_tools(None)
        .await
        .expect("list_tools must succeed");
    let mut names: Vec<String> = listed.tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();

    let mut expected: Vec<String> = FROZEN_TOOLS.iter().map(|s| (*s).to_string()).collect();
    expected.sort();

    assert_eq!(
        names, expected,
        "frozen MCP tool set drifted; got {names:?}, expected {expected:?}"
    );

    let snap_dir = snapshots_dir();
    std::fs::create_dir_all(&snap_dir).expect("mkdir snapshots dir");

    let update = std::env::var("AEROSYNC_UPDATE_SNAPSHOTS")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();

    let mut drift_report: Vec<String> = Vec::new();

    for tool in &listed.tools {
        let schema_value = Value::Object((*tool.input_schema).clone());
        // Snapshot only `description + input_schema` (the surface that
        // matters to a JSON-RPC client). `name` is locked above and is
        // implicit in the file name.
        let snapshot_payload = json!({
            "description": tool.description.as_deref().unwrap_or(""),
            "input_schema": schema_value,
        });
        let rendered = canonical_pretty(&snapshot_payload);

        let path = snap_dir.join(format!("{}.json", tool.name));
        if update || !path.exists() {
            std::fs::write(&path, &rendered)
                .unwrap_or_else(|e| panic!("failed to write snapshot {}: {e}", path.display()));
            continue;
        }

        let existing = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read snapshot {}: {e}", path.display()));
        if existing != rendered {
            drift_report.push(format!(
                "── {} ──\n--- snapshot ({})\n+++ live\n{}",
                tool.name,
                path.display(),
                line_diff(&existing, &rendered)
            ));
        }
    }

    assert!(
        drift_report.is_empty(),
        "MCP tool input_schema drift detected. Either fix the regression \
         or, if the schema change is intentional, regenerate snapshots \
         with `AEROSYNC_UPDATE_SNAPSHOTS=1 cargo test -p aerosync-mcp \
         --test mcp_tool_schema_snapshot` and review the diff in PR.\n\n{}",
        drift_report.join("\n\n")
    );

    client.cancel().await.unwrap();
}

/// Tiny line-oriented diff for human-readable drift reports — keeps
/// the regression failure self-contained without pulling in a new
/// dev-dep just for this.
fn line_diff(old: &str, new: &str) -> String {
    let mut out = String::new();
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let max = old_lines.len().max(new_lines.len());
    for i in 0..max {
        match (old_lines.get(i), new_lines.get(i)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => {
                out.push_str(&format!("- {a}\n+ {b}\n"));
            }
            (Some(a), None) => {
                out.push_str(&format!("- {a}\n"));
            }
            (None, Some(b)) => {
                out.push_str(&format!("+ {b}\n"));
            }
            (None, None) => {}
        }
    }
    if out.is_empty() {
        out.push_str("(no line-level diff; whitespace or trailing newline)\n");
    }
    out
}

// ── 3. Each tool is callable with a minimal valid argument set ───────────
//
// Heavy mutating tools (`send_file`, `send_directory`, `start_receiver`,
// `request_file`) already have dedicated coverage in
// `tests/mcp_integration.rs` — including spawning a real receiver in
// `request_file_actually_accepts_a_file`. To keep this snapshot test
// fast (< 1s) and free of network races, we exercise the seven
// state-light tools here:
//
//   * stop_receiver         — no receiver running → "no receiver" success
//   * get_receiver_status   — no receiver running → success, running=false
//   * list_history          — empty store → success, records=[]
//   * discover_receivers    — short timeout → success, possibly empty list
//   * get_transfer_status   — random uuid → success=false, "not found"
//   * wait_receipt          — random uuid → success=false, "Receipt not found"
//   * cancel_receipt        — random uuid → success=false, "Receipt not found"
//
// `send_file` / `send_directory` / `start_receiver` / `request_file`
// are deliberately delegated to `mcp_integration.rs` and noted in the
// per-test comments below.

fn parse_text_response(result: &rmcp::model::CallToolResult) -> Value {
    let raw = result
        .content
        .first()
        .and_then(|c| c.raw.as_text())
        .map(|t| t.text.as_str())
        .expect("text content");
    serde_json::from_str(raw).expect("response body is JSON")
}

#[tokio::test]
async fn light_tools_are_callable_with_minimal_args() {
    let dir = tempdir().unwrap();
    // Isolate so list_history can't see the developer's real history file.
    let server = AeroSyncMcpServer::new().with_aerosync_dir(dir.path().to_path_buf());
    let (client, _h) = spawn_pair(server).await;

    // ── stop_receiver: nothing running → stable "No receiver is running"
    //    documented error (success=false is the contract here, not a bug).
    let resp = client
        .call_tool(CallToolRequestParams::new("stop_receiver"))
        .await
        .expect("stop_receiver call");
    let parsed = parse_text_response(&resp);
    assert_eq!(parsed["success"], json!(false));
    assert!(
        parsed["error"]
            .as_str()
            .unwrap_or("")
            .contains("No receiver is running"),
        "stop_receiver no-op error wording drifted: {parsed}"
    );

    // ── get_receiver_status: not running → success, running=false. ──
    let resp = client
        .call_tool(CallToolRequestParams::new("get_receiver_status"))
        .await
        .expect("get_receiver_status call");
    let parsed = parse_text_response(&resp);
    assert_eq!(parsed["success"], json!(true));
    assert_eq!(parsed["data"]["running"], json!(false));

    // ── list_history: empty store → success, no records. ──
    let resp = client
        .call_tool(CallToolRequestParams::new("list_history"))
        .await
        .expect("list_history call");
    let parsed = parse_text_response(&resp);
    assert_eq!(parsed["success"], json!(true));
    let records = &parsed["data"]["records"];
    assert!(
        records.is_array(),
        "list_history must return a `records` array, got {parsed}"
    );
    let rr = &parsed["data"]["recoverable_receipts"];
    assert!(
        rr.is_array(),
        "list_history must return a `recoverable_receipts` array, got {parsed}"
    );

    // ── discover_receivers: short timeout, must complete fast and
    //    return a JSON array of peers (typically empty in CI). ──
    let args = json!({ "timeout": 1 }).as_object().unwrap().clone();
    let resp = client
        .call_tool(CallToolRequestParams::new("discover_receivers").with_arguments(args))
        .await
        .expect("discover_receivers call");
    let parsed = parse_text_response(&resp);
    assert_eq!(parsed["success"], json!(true));
    assert!(
        parsed["data"]["receivers"].is_array() || parsed["data"]["count"].is_number(),
        "discover_receivers response shape changed: {parsed}"
    );

    // ── get_transfer_status: random uuid → stable "not found" error. ──
    let args = json!({ "task_id": uuid::Uuid::new_v4().to_string() })
        .as_object()
        .unwrap()
        .clone();
    let resp = client
        .call_tool(CallToolRequestParams::new("get_transfer_status").with_arguments(args))
        .await
        .expect("get_transfer_status call");
    let parsed = parse_text_response(&resp);
    assert_eq!(parsed["success"], json!(false));
    assert!(
        parsed["error"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("not found"),
        "get_transfer_status unknown id should contain 'not found': {parsed}"
    );

    // ── wait_receipt: random uuid + 0ms timeout → "Receipt not found". ──
    let args = json!({
        "receipt_id": uuid::Uuid::new_v4().to_string(),
        "timeout_ms": 0u64,
    })
    .as_object()
    .unwrap()
    .clone();
    let resp = client
        .call_tool(CallToolRequestParams::new("wait_receipt").with_arguments(args))
        .await
        .expect("wait_receipt call");
    let parsed = parse_text_response(&resp);
    assert_eq!(parsed["success"], json!(false));
    assert_eq!(
        parsed["error"].as_str().unwrap_or(""),
        "Receipt not found",
        "wait_receipt unknown id error wording drifted"
    );

    // ── cancel_receipt: random uuid → "Receipt not found". ──
    let args = json!({
        "receipt_id": uuid::Uuid::new_v4().to_string(),
        "reason": "snapshot-test",
    })
    .as_object()
    .unwrap()
    .clone();
    let resp = client
        .call_tool(CallToolRequestParams::new("cancel_receipt").with_arguments(args))
        .await
        .expect("cancel_receipt call");
    let parsed = parse_text_response(&resp);
    assert_eq!(parsed["success"], json!(false));
    assert_eq!(
        parsed["error"].as_str().unwrap_or(""),
        "Receipt not found",
        "cancel_receipt unknown id error wording drifted"
    );

    client.cancel().await.unwrap();
}
