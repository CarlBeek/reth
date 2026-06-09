//! Strict configuration for the `ClickHouse` export pipeline.
//!
//! Export is opt-in: it is enabled only when a config file path is supplied
//! (`--research.export-config-path`). The file is parsed strictly — unknown
//! keys are rejected — so a typo fails fast at startup rather than silently
//! disabling a setting.
//!
//! The password is never read from the file or the command line (process
//! command lines are observable). It is resolved once at startup from the
//! environment variable named by `password_env` and wrapped in [`Secret`] so it
//! cannot leak through `Debug`/log output.

use serde::Deserialize;
use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;

fn default_database() -> String {
    "gas_analysis".to_string()
}
const fn default_request_timeout_secs() -> u64 {
    30
}
const fn default_poll_interval_ms() -> u64 {
    1000
}
const fn default_retry_initial_ms() -> u64 {
    500
}
const fn default_retry_max_secs() -> u64 {
    60
}
const fn default_max_batch_rows() -> usize {
    1000
}
const fn default_max_batch_bytes() -> usize {
    8 * 1024 * 1024
}
const fn default_max_single_row_bytes() -> usize {
    16 * 1024 * 1024
}
const fn default_exported_retention_secs() -> u64 {
    7 * 24 * 60 * 60
}
const fn default_max_pending_bytes() -> u64 {
    50 * 1024 * 1024 * 1024
}

/// Errors raised while loading or validating the export configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configured path does not exist.
    #[error("export config file not found: {}", .0.display())]
    NotFound(PathBuf),
    /// The config file could not be read.
    #[error("failed to read export config {}: {source}", path.display())]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The config file could not be parsed as TOML (or had unknown keys).
    #[error("failed to parse export config {}: {source}", path.display())]
    Parse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying TOML error.
        source: toml::de::Error,
    },
    /// The endpoint is not `https://`.
    #[error("export endpoint must be https:// (got {0:?})")]
    InsecureEndpoint(String),
    /// The password environment variable is unset.
    #[error("password env var {0} is not set")]
    MissingPassword(String),
    /// A SQL identifier (database name) contains unsafe characters.
    #[error("invalid {field} identifier {value:?}: only [A-Za-z0-9_] allowed")]
    InvalidIdentifier {
        /// Which field failed validation.
        field: &'static str,
        /// The rejected value.
        value: String,
    },
    /// The CA certificate file could not be read.
    #[error("failed to read CA cert {}: {source}", path.display())]
    CaCert {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

/// A resolved secret (the `ClickHouse` password) that never appears in `Debug` or
/// log output. Use [`Secret::expose`] only at the point of use.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Borrow the underlying secret. Callers must not log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Raw TOML shape. Parsed strictly (`deny_unknown_fields`) then validated into
/// [`ExportConfig`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    endpoint: String,
    #[serde(default = "default_database")]
    database: String,
    username: String,
    password_env: String,
    #[serde(default = "default_request_timeout_secs")]
    request_timeout_secs: u64,
    #[serde(default = "default_poll_interval_ms")]
    poll_interval_ms: u64,
    #[serde(default = "default_retry_initial_ms")]
    retry_initial_ms: u64,
    #[serde(default = "default_retry_max_secs")]
    retry_max_secs: u64,
    #[serde(default = "default_max_batch_rows")]
    max_batch_rows: usize,
    #[serde(default = "default_max_batch_bytes")]
    max_batch_bytes: usize,
    #[serde(default = "default_max_single_row_bytes")]
    max_single_row_bytes: usize,
    #[serde(default = "default_exported_retention_secs")]
    exported_retention_secs: u64,
    #[serde(default = "default_max_pending_bytes")]
    max_pending_bytes: u64,
    #[serde(default)]
    ca_cert_path: Option<PathBuf>,
}

/// Validated export configuration. The password is resolved and the CA cert (if
/// any) is read into memory at load time.
#[derive(Clone)]
pub struct ExportConfig {
    /// Base `ClickHouse` HTTPS endpoint (e.g. `https://host:8443`).
    pub endpoint: String,
    /// Destination database name (validated identifier).
    pub database: String,
    /// Ingest username.
    pub username: String,
    /// Resolved password (redacted in logs).
    pub password: Secret,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Idle poll interval when the outbox has nothing due.
    pub poll_interval: Duration,
    /// Initial retry backoff.
    pub retry_initial: Duration,
    /// Maximum retry backoff.
    pub retry_max: Duration,
    /// Maximum rows per insert request.
    pub max_batch_rows: usize,
    /// Maximum encoded bytes per insert request.
    pub max_batch_bytes: usize,
    /// Hard limit above which a single row is permanently blocked.
    pub max_single_row_bytes: usize,
    /// How long exported audit rows are retained before pruning.
    pub exported_retention: Duration,
    /// Backlog ceiling: when pending bytes exceed this, the worker fails loudly.
    pub max_pending_bytes: u64,
    /// PEM bytes of a private CA certificate, if configured.
    pub ca_cert_pem: Option<Vec<u8>>,
}

impl fmt::Debug for ExportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportConfig")
            .field("endpoint", &self.endpoint)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &self.password)
            .field("request_timeout", &self.request_timeout)
            .field("max_batch_rows", &self.max_batch_rows)
            .field("max_batch_bytes", &self.max_batch_bytes)
            .field("max_single_row_bytes", &self.max_single_row_bytes)
            .field("max_pending_bytes", &self.max_pending_bytes)
            .field("ca_cert", &self.ca_cert_pem.as_ref().map(|_| "<provided>"))
            .finish()
    }
}

impl ExportConfig {
    /// Load and validate the config from a TOML file, resolving the password
    /// from the environment. Requires an `https://` endpoint.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Err(ConfigError::NotFound(path.to_path_buf()));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
        let raw: RawConfig = toml::from_str(&text)
            .map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source })?;
        Self::from_raw(raw, true)
    }

    fn from_raw(raw: RawConfig, require_https: bool) -> Result<Self, ConfigError> {
        validate_identifier("database", &raw.database)?;

        if require_https && !raw.endpoint.starts_with("https://") {
            return Err(ConfigError::InsecureEndpoint(raw.endpoint));
        }

        let password = std::env::var(&raw.password_env)
            .map_err(|_| ConfigError::MissingPassword(raw.password_env.clone()))?;

        let ca_cert_pem = match &raw.ca_cert_path {
            Some(p) => Some(
                std::fs::read(p)
                    .map_err(|source| ConfigError::CaCert { path: p.clone(), source })?,
            ),
            None => None,
        };

        Ok(Self {
            endpoint: raw.endpoint.trim_end_matches('/').to_string(),
            database: raw.database,
            username: raw.username,
            password: Secret(password),
            request_timeout: Duration::from_secs(raw.request_timeout_secs),
            poll_interval: Duration::from_millis(raw.poll_interval_ms),
            retry_initial: Duration::from_millis(raw.retry_initial_ms),
            retry_max: Duration::from_secs(raw.retry_max_secs),
            max_batch_rows: raw.max_batch_rows.max(1),
            max_batch_bytes: raw.max_batch_bytes.max(1),
            max_single_row_bytes: raw.max_single_row_bytes.max(1),
            exported_retention: Duration::from_secs(raw.exported_retention_secs),
            max_pending_bytes: raw.max_pending_bytes,
            ca_cert_pem,
        })
    }
}

/// Validate that a value is a safe SQL identifier (`[A-Za-z0-9_]+`) before it is
/// interpolated into a query. Destination table names are code constants, but
/// the database name comes from config and must be checked.
pub fn validate_identifier(field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ConfigError::InvalidIdentifier { field, value: value.to_string() });
    }
    Ok(())
}

#[cfg(test)]
impl ExportConfig {
    /// Test-only constructor that skips the `https://` requirement and takes the
    /// password directly. Used by worker/integration tests against a local
    /// scripted listener.
    pub(crate) fn insecure_for_test(endpoint: &str, database: &str, password: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            database: database.to_string(),
            username: "ingest".to_string(),
            password: Secret(password.to_string()),
            request_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(20),
            retry_initial: Duration::from_millis(500),
            retry_max: Duration::from_secs(60),
            max_batch_rows: 1000,
            max_batch_bytes: 8 * 1024 * 1024,
            max_single_row_bytes: 16 * 1024 * 1024,
            exported_retention: Duration::from_secs(604_800),
            max_pending_bytes: 50 * 1024 * 1024 * 1024,
            ca_cert_pem: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(endpoint: &str, database: &str, password_env: &str) -> RawConfig {
        RawConfig {
            endpoint: endpoint.to_string(),
            database: database.to_string(),
            username: "ingest".to_string(),
            password_env: password_env.to_string(),
            request_timeout_secs: default_request_timeout_secs(),
            poll_interval_ms: default_poll_interval_ms(),
            retry_initial_ms: default_retry_initial_ms(),
            retry_max_secs: default_retry_max_secs(),
            max_batch_rows: default_max_batch_rows(),
            max_batch_bytes: default_max_batch_bytes(),
            max_single_row_bytes: default_max_single_row_bytes(),
            exported_retention_secs: default_exported_retention_secs(),
            max_pending_bytes: default_max_pending_bytes(),
            ca_cert_path: None,
        }
    }

    #[test]
    fn rejects_unknown_keys() {
        let toml = r#"
            endpoint = "https://x:8443"
            database = "default"
            username = "ingest"
            password_env = "PW"
            bogus_key = 1
        "#;
        assert!(toml::from_str::<RawConfig>(toml).is_err());
    }

    #[test]
    fn database_defaults_to_gas_analysis_when_omitted() {
        let toml = r#"
            endpoint = "https://x:8443"
            username = "gas_analysis"
            password_env = "PW"
        "#;
        let raw: RawConfig = toml::from_str(toml).unwrap();
        assert_eq!(raw.database, "gas_analysis");
    }

    #[test]
    fn rejects_non_https_endpoint() {
        // SAFETY: test-only mutation of a process-unique env var.
        unsafe { std::env::set_var("PW_HTTP", "secret") };
        let err =
            ExportConfig::from_raw(raw("http://x:8123", "default", "PW_HTTP"), true).unwrap_err();
        assert!(matches!(err, ConfigError::InsecureEndpoint(_)));
    }

    #[test]
    fn rejects_invalid_database_identifier() {
        let err = ExportConfig::from_raw(raw("https://x:8443", "bad name;DROP", "PW2"), true)
            .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidIdentifier { field: "database", .. }));
    }

    #[test]
    fn missing_password_env_is_an_error() {
        let err = ExportConfig::from_raw(
            raw("https://x:8443", "default", "DEFINITELY_UNSET_VAR_XYZ"),
            true,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::MissingPassword(_)));
    }

    #[test]
    fn resolves_password_and_redacts_it() {
        // SAFETY: test-only mutation of a process-unique env var.
        unsafe { std::env::set_var("PW_OK", "hunter2") };
        let config =
            ExportConfig::from_raw(raw("https://x:8443/", "default", "PW_OK"), true).unwrap();
        assert_eq!(config.password.expose(), "hunter2");
        // Endpoint trailing slash trimmed.
        assert_eq!(config.endpoint, "https://x:8443");
        // Neither Debug rendering leaks the secret.
        assert!(!format!("{config:?}").contains("hunter2"));
        assert!(!format!("{:?}", config.password).contains("hunter2"));
    }

    #[test]
    fn insecure_endpoint_allowed_when_not_required() {
        // SAFETY: test-only mutation of a process-unique env var.
        unsafe { std::env::set_var("PW_LOCAL", "x") };
        let config =
            ExportConfig::from_raw(raw("http://127.0.0.1:8123", "default", "PW_LOCAL"), false);
        assert!(config.is_ok());
    }
}
