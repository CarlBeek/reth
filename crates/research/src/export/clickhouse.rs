//! `ClickHouse` HTTPS client: one reusable `reqwest::Client` that performs
//! `JSONEachRow` inserts and the startup `DESCRIBE TABLE` schema check.
//!
//! Failures are classified so the worker can react correctly:
//! - **transient** (timeout, connection reset, 408/425/429, 5xx) → retry with backoff;
//! - **permanent** (400/404, unknown table/column, oversized row, malformed response) → block the
//!   item, it cannot succeed by retrying;
//! - **auth/config** (401/403) → logged at error level but left *retryable*, so corrected
//!   credentials recover without data loss.

use crate::export::config::ExportConfig;
use std::collections::HashSet;
use thiserror::Error;

/// Destination table for `gas_analysis_run`.
pub const RUN_TABLE: &str = "gas_analysis_run";
/// Destination table for `gas_analysis_block_coverage`.
pub const COVERAGE_TABLE: &str = "gas_analysis_block_coverage";
/// Destination table for `gas_analysis_block_summary`.
pub const SUMMARY_TABLE: &str = "gas_analysis_block_summary";
/// Destination table for `gas_analysis_divergence`.
pub const DIVERGENCE_TABLE: &str = "gas_analysis_divergence";
/// Destination table for `gas_analysis_tx_gas_result`.
pub const TX_GAS_RESULT_TABLE: &str = "gas_analysis_tx_gas_result";

/// Maximum bytes of a remote error response body included in an error message.
const ERROR_BODY_SNIPPET_BYTES: usize = 500;

/// The five destination tables. Names are code constants — never config — so a
/// malformed config can't redirect inserts to an attacker-chosen table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationTable {
    /// `gas_analysis_run`
    Run,
    /// `gas_analysis_block_coverage`
    Coverage,
    /// `gas_analysis_block_summary`
    Summary,
    /// `gas_analysis_divergence`
    Divergence,
    /// `gas_analysis_tx_gas_result`
    TxGasResult,
}

impl DestinationTable {
    /// Every destination table, in insert order (run, divergence, summary,
    /// coverage — coverage last is enforced by the worker, not this list).
    pub const ALL: [Self; 5] =
        [Self::Run, Self::Coverage, Self::Summary, Self::Divergence, Self::TxGasResult];

    /// The `ClickHouse` table name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Run => RUN_TABLE,
            Self::Coverage => COVERAGE_TABLE,
            Self::Summary => SUMMARY_TABLE,
            Self::Divergence => DIVERGENCE_TABLE,
            Self::TxGasResult => TX_GAS_RESULT_TABLE,
        }
    }

    /// Columns this producer writes. The startup schema check requires every
    /// one to be present in the live table (extra columns in the table are
    /// allowed). This is the producer's half of the column contract; the
    /// canonical DDL lives in `bin/reth-research/clickhouse/migrations`.
    pub const fn required_columns(self) -> &'static [&'static str] {
        match self {
            Self::Run => &[
                "updated_at",
                "analysis_config_hash",
                "chain_id",
                "producer_schema_version",
                "producer_git_commit",
                "replay_semantics",
                "manifest_json",
            ],
            Self::Coverage => &[
                "updated_at",
                "row_id",
                "analysis_config_hash",
                "chain_id",
                "producer_schema_version",
                "producer_git_commit",
                "replay_semantics",
                "schedule_name",
                "schedule_config_hash",
                "block_number",
                "block_hash",
                "parent_hash",
                "block_timestamp",
                "tx_count",
                "tx_count_unchanged",
                "tx_count_gas_only",
                "tx_count_stored",
                "block_gas_used",
                "block_gas_limit",
                "block_base_fee_per_gas",
                "expected_drill_in_count",
                "retained_drill_in_count",
                "drill_ins_truncated",
            ],
            Self::TxGasResult => &[
                "updated_at",
                "row_id",
                "analysis_config_hash",
                "chain_id",
                "producer_schema_version",
                "producer_git_commit",
                "replay_semantics",
                "schedule_name",
                "schedule_config_hash",
                "block_number",
                "block_hash",
                "block_timestamp",
                "tx_index",
                "tx_hash",
                "tx_type",
                "tx_gas_limit",
                "max_fee_per_gas",
                "max_priority_fee_per_gas",
                "baseline_success",
                "baseline_gas_used",
                "baseline_total_gas_spent",
                "schedule_success",
                "schedule_gas_used",
                "schedule_total_gas_spent",
                "schedule_gas_refunded",
                "schedule_floor_gas",
                "schedule_state_gas_spent",
                "schedule_intrinsic_gas",
                "min_multiplier_to_succeed",
            ],
            Self::Summary => &[
                "updated_at",
                "row_id",
                "analysis_config_hash",
                "chain_id",
                "producer_schema_version",
                "producer_git_commit",
                "replay_semantics",
                "schedule_name",
                "schedule_config_hash",
                "block_number",
                "block_hash",
                "block_timestamp",
                "class",
                "tx_count",
                "gas_delta_sum",
                "gas_delta_sum_sq",
                "gas_delta_min",
                "gas_delta_max",
                "gas_delta_log2_hist",
                "opcode",
                "opcode_count",
                "opcode_gas_baseline",
                "opcode_gas_schedule",
                "state_gas_sum",
                "state_gas_spillover_sum",
                "multiplier_log2_hist",
                "tx_count_creation",
                "tx_count_authorization",
                "tx_count_runtime_state",
                "tx_count_no_state",
                "cold_account_access_count",
                "sload_cold_count",
                "sload_warm_count",
                "sstore_cold_count",
                "sstore_set_count",
                "sstore_reset_count",
                "sstore_clear_count",
                "sstore_noop_count",
                "sstore_dirty_count",
                "warm_account_access_count",
                "value_transfer_count",
                "create_opcode_count",
                "access_list_address_count",
                "access_list_storage_key_count",
                "tx_count_type_legacy",
                "tx_count_type_access_list",
                "tx_count_type_dynamic_fee",
                "tx_count_type_blob",
                "tx_count_type_set_code",
                "tx_count_type_other",
                "tx_count_simple_transfer",
                "tx_count_contract_call",
                "gas_delta_pct_hist",
                "baseline_gas_used_sum",
            ],
            Self::Divergence => &[
                "updated_at",
                "row_id",
                "analysis_config_hash",
                "chain_id",
                "producer_schema_version",
                "producer_git_commit",
                "replay_semantics",
                "schedule_name",
                "schedule_config_hash",
                "block_number",
                "block_hash",
                "block_timestamp",
                "tx_index",
                "tx_hash",
                "sender",
                "recipient",
                "is_create",
                "tx_gas_limit",
                "baseline_success",
                "schedule_success",
                "status_changed",
                "event_logs_changed",
                "output_changed",
                "logs_bloom_changed",
                "baseline_gas_used",
                "schedule_gas_used",
                "gas_delta",
                "baseline_total_gas_spent",
                "baseline_gas_refunded",
                "schedule_total_gas_spent",
                "schedule_gas_refunded",
                "schedule_intrinsic_gas",
                "schedule_floor_gas",
                "outer_limit_only_failure",
                "min_multiplier_to_succeed",
                "divergence_contract",
                "divergence_pc",
                "divergence_call_depth",
                "divergence_opcode",
                "oog_contract",
                "oog_pc",
                "oog_call_depth",
                "oog_opcode",
                "oog_pattern",
                "oog_gas_remaining",
                "oog_chain_proportional",
                "oog_bottleneck_depth",
                "oog_bottleneck_kind",
                "schedule_state_gas_spent",
                "schedule_state_gas_demanded",
                "schedule_initial_state_gas",
                "schedule_initial_reservoir",
                "runtime_state_gas",
                "runtime_state_gas_spillover",
                "state_gas_category",
                "reservoir_exhausted",
                "replay_halt_oog",
                "cold_account_access_count",
                "additional_gas_charged",
                "failure_selector_path",
                "tx_type",
                "tx_nonce",
                "entry_selector",
                "input_zero_bytes",
                "input_nonzero_bytes",
                "has_authorization",
                "failure_reason",
                "revert_data",
                "revert_decoded",
                "tx_output",
                "baseline_frame_success",
                "baseline_frame_gas_used",
                "baseline_frame_gas_provided",
                "surcharge_at_oog",
                "gas_div_contract",
                "gas_div_pc",
                "gas_div_call_depth",
                "gas_div_opcode",
                "tax_second_db_read",
                "tax_other",
                "tax_intrinsic",
                "sload_cold_count",
                "sload_warm_count",
                "sstore_cold_count",
                "sstore_set_count",
                "sstore_reset_count",
                "sstore_clear_count",
                "sstore_noop_count",
                "sstore_dirty_count",
                "warm_account_access_count",
                "value_transfer_count",
                "create_opcode_count",
                "access_list_address_count",
                "access_list_storage_key_count",
                "tier1_failure_reason",
                "tier1_oog_opcode",
                "tier1_oog_contract",
                "tier1_oog_pc",
                "tier1_oog_depth",
                "tier1_oog_gas_remaining",
                "tier1_failing_selector",
                "tier1_failing_gas_provided",
                "tier1_failing_gas_requested",
                "trace_payload",
                "trace_content_hash",
                "trace_uncompressed_size_bytes",
                "trace_format",
                "trace_format_version",
                "call_frame_count",
                "opcode_count_row_count",
                "baseline_event_log_count",
                "schedule_event_log_count",
                "opcode_capture_complete",
            ],
        }
    }
}

/// A classified `ClickHouse` request failure.
#[derive(Debug, Error)]
pub enum ClickHouseError {
    /// Retry with backoff (network/timeout/408/425/429/5xx).
    #[error("transient ClickHouse failure: {0}")]
    Transient(String),
    /// Cannot succeed by retrying (400/404, unknown table/column, oversized,
    /// malformed response).
    #[error("permanent ClickHouse failure: {0}")]
    Permanent(String),
    /// Credentials/permissions (401/403). Logged at error level but retryable.
    #[error("ClickHouse auth/config failure: {0}")]
    Auth(String),
}

impl ClickHouseError {
    /// Whether the worker should retry the item (transient or auth) rather than
    /// block it permanently.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::Auth(_))
    }
}

/// Reusable HTTPS client for one `ClickHouse` endpoint/database.
#[derive(Debug, Clone)]
pub struct ClickHouseClient {
    http: reqwest::Client,
    endpoint: String,
    database: String,
    username: String,
    password: String,
}

impl ClickHouseClient {
    /// Build the client from validated config, installing the optional private
    /// CA. TLS defaults to rustls (the only backend compiled in).
    pub fn new(config: &ExportConfig) -> Result<Self, ClickHouseError> {
        let mut builder = reqwest::Client::builder().timeout(config.request_timeout);
        if let Some(pem) = &config.ca_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem)
                .map_err(|e| ClickHouseError::Auth(format!("invalid CA certificate: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| ClickHouseError::Auth(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            http,
            endpoint: config.endpoint.clone(),
            database: config.database.clone(),
            username: config.username.clone(),
            password: config.password.expose().to_string(),
        })
    }

    /// Insert newline-delimited `JSONEachRow` rows into one table. `dedup_token`
    /// is a deterministic per-table/chunk token enabling `ClickHouse` insert
    /// deduplication where supported. (The
    /// [`ClickHouseSink`](crate::export::worker::ClickHouseSink) trait method forwards here.)
    pub async fn post_rows(
        &self,
        table: DestinationTable,
        body: String,
        dedup_token: String,
    ) -> Result<(), ClickHouseError> {
        let query = format!("INSERT INTO {}.{} FORMAT JSONEachRow", self.database, table.name());
        self.execute(&query, body, Some(&dedup_token)).await.map(|_| ())
    }

    /// Run the startup schema check: for each table, `DESCRIBE TABLE` and verify
    /// every required column is present. A mismatch is permanent and fails
    /// startup.
    pub async fn check_schema(&self) -> Result<(), ClickHouseError> {
        for table in DestinationTable::ALL {
            let present: HashSet<String> =
                self.describe_columns(table).await?.into_iter().collect();
            for required in table.required_columns() {
                if !present.contains(*required) {
                    return Err(ClickHouseError::Permanent(format!(
                        "table {} is missing required column `{}`",
                        table.name(),
                        required
                    )));
                }
            }
        }
        Ok(())
    }

    async fn describe_columns(
        &self,
        table: DestinationTable,
    ) -> Result<Vec<String>, ClickHouseError> {
        let query = format!("DESCRIBE TABLE {}.{} FORMAT JSONEachRow", self.database, table.name());
        let text = self.execute(&query, String::new(), None).await?;
        let mut columns = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                ClickHouseError::Permanent(format!("malformed DESCRIBE response: {e}"))
            })?;
            if let Some(name) = value.get("name").and_then(|n| n.as_str()) {
                columns.push(name.to_string());
            }
        }
        Ok(columns)
    }

    async fn execute(
        &self,
        query: &str,
        body: String,
        dedup_token: Option<&str>,
    ) -> Result<String, ClickHouseError> {
        let mut request = self
            .http
            .post(&self.endpoint)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("query", query), ("database", &self.database)]);
        if let Some(token) = dedup_token {
            request = request.query(&[("insert_deduplication_token", token)]);
        }

        let response = request.body(body).send().await.map_err(classify_transport_error)?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(text)
        } else {
            Err(classify_status(status.as_u16(), &text))
        }
    }
}

fn classify_transport_error(err: reqwest::Error) -> ClickHouseError {
    // Network-layer failures (timeouts, resets, connect errors) are transient:
    // the endpoint may recover. We bias toward retry rather than blocking data.
    ClickHouseError::Transient(format!("request transport error: {err}"))
}

fn classify_status(status: u16, body: &str) -> ClickHouseError {
    let snippet = snippet(body);
    match status {
        408 | 425 | 429 | 500..=599 => {
            ClickHouseError::Transient(format!("HTTP {status}: {snippet}"))
        }
        401 | 403 => ClickHouseError::Auth(format!("HTTP {status}: {snippet}")),
        400 | 404 => ClickHouseError::Permanent(format!("HTTP {status}: {snippet}")),
        _ => ClickHouseError::Permanent(format!("unexpected HTTP {status}: {snippet}")),
    }
}

fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= ERROR_BODY_SNIPPET_BYTES {
        trimmed.to_string()
    } else {
        let mut end = ERROR_BODY_SNIPPET_BYTES;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification() {
        assert!(matches!(classify_status(429, "busy"), ClickHouseError::Transient(_)));
        assert!(matches!(classify_status(503, "down"), ClickHouseError::Transient(_)));
        assert!(matches!(classify_status(401, "nope"), ClickHouseError::Auth(_)));
        assert!(matches!(classify_status(403, "nope"), ClickHouseError::Auth(_)));
        assert!(matches!(classify_status(400, "bad"), ClickHouseError::Permanent(_)));
        assert!(matches!(classify_status(404, "missing"), ClickHouseError::Permanent(_)));
    }

    #[test]
    fn auth_and_transient_are_retryable_permanent_is_not() {
        assert!(ClickHouseError::Transient("x".into()).is_retryable());
        assert!(ClickHouseError::Auth("x".into()).is_retryable());
        assert!(!ClickHouseError::Permanent("x".into()).is_retryable());
    }

    #[test]
    fn error_body_snippet_is_bounded() {
        let big = "e".repeat(5000);
        let err = classify_status(400, &big);
        let msg = err.to_string();
        assert!(msg.len() < 600, "snippet should be bounded, got {} bytes", msg.len());
        assert!(msg.contains('…'));
    }

    #[test]
    fn table_names_are_constants() {
        assert_eq!(DestinationTable::Run.name(), "gas_analysis_run");
        assert_eq!(DestinationTable::Coverage.name(), "gas_analysis_block_coverage");
        assert_eq!(DestinationTable::Summary.name(), "gas_analysis_block_summary");
        assert_eq!(DestinationTable::Divergence.name(), "gas_analysis_divergence");
    }

    #[test]
    fn invalid_ca_cert_fails_client_build() {
        let mut config = ExportConfig::insecure_for_test("http://127.0.0.1:1", "default", "pw");
        config.ca_cert_pem = Some(b"-----BEGIN CERTIFICATE-----\nnot base64\n".to_vec());
        let err = ClickHouseClient::new(&config).unwrap_err();
        assert!(matches!(err, ClickHouseError::Auth(_)));
    }

    // ── Live-socket tests against a scripted TcpListener (no mock-server dep) ──

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Accept one connection, read the full HTTP request (headers + body by
    /// Content-Length), reply with `response`, and return the captured request.
    async fn scripted_server(
        response: &'static [u8],
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = stream.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find(&buf, b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let content_len = header
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + content_len {
                        break;
                    }
                }
            }
            let _ = stream.write_all(response).await;
            let _ = stream.flush().await;
            buf
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn client_posts_jsoneachrow_with_auth_query_and_body() {
        let (addr, handle) =
            scripted_server(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        let config =
            ExportConfig::insecure_for_test(&format!("http://{addr}"), "default", "secret");
        let client = ClickHouseClient::new(&config).unwrap();
        client
            .post_rows(DestinationTable::Coverage, "{\"a\":1}".to_string(), "tok-1".to_string())
            .await
            .unwrap();

        let req = String::from_utf8_lossy(&handle.await.unwrap()).to_string();
        assert!(req.starts_with("POST "), "expected POST, got: {}", &req[..req.len().min(40)]);
        assert!(req.contains("query=INSERT+INTO+default.gas_analysis_block_coverage"));
        assert!(req.contains("insert_deduplication_token=tok-1"));
        assert!(req.to_lowercase().contains("authorization: basic "));
        assert!(req.contains("{\"a\":1}"));
    }

    #[tokio::test]
    async fn http_429_classified_transient() {
        let (addr, _h) = scripted_server(
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy",
        )
        .await;
        let config = ExportConfig::insecure_for_test(&format!("http://{addr}"), "default", "pw");
        let client = ClickHouseClient::new(&config).unwrap();
        let err = client
            .post_rows(DestinationTable::Run, "{}".to_string(), "t".to_string())
            .await
            .unwrap_err();
        assert!(matches!(err, ClickHouseError::Transient(_)));
    }

    #[tokio::test]
    async fn http_400_classified_permanent() {
        let (addr, _h) = scripted_server(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 7\r\nConnection: close\r\n\r\nbad sql",
        )
        .await;
        let config = ExportConfig::insecure_for_test(&format!("http://{addr}"), "default", "pw");
        let client = ClickHouseClient::new(&config).unwrap();
        let err = client
            .post_rows(DestinationTable::Run, "{}".to_string(), "t".to_string())
            .await
            .unwrap_err();
        assert!(matches!(err, ClickHouseError::Permanent(_)));
    }

    #[tokio::test]
    async fn connection_drop_classified_transient() {
        // Accept then close without responding → reqwest sees an aborted
        // response, which we treat as a retryable (ambiguous) failure.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut tmp = [0u8; 1024];
            let _ = stream.read(&mut tmp).await;
            drop(stream);
        });
        let config = ExportConfig::insecure_for_test(&format!("http://{addr}"), "default", "pw");
        let client = ClickHouseClient::new(&config).unwrap();
        let err = client
            .post_rows(DestinationTable::Run, "{}".to_string(), "t".to_string())
            .await
            .unwrap_err();
        assert!(matches!(err, ClickHouseError::Transient(_)));
    }
}
