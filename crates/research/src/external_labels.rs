//! External-API contract-label and function-signature backfill.
//!
//! Periodic tasks that enrich the research `SQLite` DB with labels that
//! can't be derived from on-chain bytecode alone:
//!
//! - `contract_labels`: per-address contract names and curated protocol tags (e.g. "USDC", "Uniswap
//!   V3: Router"). Fetched from Blockscout v2 → Sourcify → Etherscan v2, in that order, on a
//!   per-address fallback chain.
//! - `function_signatures`: per-selector Solidity signatures (e.g. `transfer(address,uint256)`).
//!   Fetched from `OpenChain`'s signature database in batches.
//!
//! Each fetcher rate-limits itself to the documented free-tier ceiling
//! of its source so the orchestrator can sustain the maximum allowed
//! throughput without operator intervention.

use crate::database::{DatabaseError, DivergenceDatabase};
use alloy_primitives::Address;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// `source` column value when Blockscout filled the row.
pub const SOURCE_BLOCKSCOUT: &str = "blockscout";
/// `source` column value when Sourcify filled the row.
pub const SOURCE_SOURCIFY: &str = "sourcify";
/// `source` column value when Etherscan filled the row.
pub const SOURCE_ETHERSCAN: &str = "etherscan";
/// `source` column value when every source missed; the row is a
/// negative-cache marker so future ticks don't re-query the address.
pub const SOURCE_NONE: &str = "none";
/// `source` column value when `OpenChain` resolved the selector.
pub const SOURCE_OPENCHAIN: &str = "openchain";

/// Default Blockscout v2 endpoint for Ethereum mainnet.
pub const DEFAULT_BLOCKSCOUT_BASE: &str = "https://eth.blockscout.com/api/v2";
/// Default Sourcify server endpoint (chain-agnostic; chain id is
/// embedded in the URL path).
pub const DEFAULT_SOURCIFY_BASE: &str = "https://sourcify.dev/server";
/// Default Etherscan v2 endpoint (one URL serves all chains via
/// `?chainid=`).
pub const DEFAULT_ETHERSCAN_BASE: &str = "https://api.etherscan.io/v2/api";
/// Default `OpenChain` signature-database v1 endpoint.
pub const DEFAULT_OPENCHAIN_BASE: &str = "https://api.openchain.xyz/signature-database/v1";
/// Default chain id for the label lookup (Ethereum mainnet).
pub const DEFAULT_CHAIN_ID: u64 = 1;

// Minimum interval between consecutive requests to a single source.
// Etherscan caps free-tier at 5 req/s; Blockscout / Sourcify aren't
// documented but tolerate ~10 req/s in practice; OpenChain is
// effectively unconstrained but we throttle to 5 req/s out of
// courtesy. Each fetcher tracks its own last-call instant.
const ETHERSCAN_MIN_INTERVAL: Duration = Duration::from_millis(200);
const BLOCKSCOUT_MIN_INTERVAL: Duration = Duration::from_millis(100);
const SOURCIFY_MIN_INTERVAL: Duration = Duration::from_millis(100);
const OPENCHAIN_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// `OpenChain` accepts comma-separated selectors in a single GET. Cap
/// batches so request URLs stay well under typical 8 KB limits — each
/// selector is 11 bytes including the comma.
const OPENCHAIN_BATCH_SIZE: usize = 64;

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// Decoded label for one contract address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractLabel {
    /// Solidity contract class name (e.g. `FiatTokenV2_2`). Populated
    /// by any source that surfaces verified-source metadata.
    pub contract_name: Option<String>,
    /// Curated protocol / brand label (e.g. "USDC", "Uniswap V3
    /// Router"). Only Blockscout's `public_tags` endpoint currently
    /// surfaces these at the API layer.
    pub protocol_tag: Option<String>,
    /// True if the source identified this address as a proxy contract.
    pub is_proxy: bool,
    /// Implementation address for proxies, when the source reports it.
    pub impl_address: Option<Address>,
}

impl ContractLabel {
    /// Whether the label is informationally empty — i.e. nothing worth
    /// storing past "we asked and got nothing." The orchestrator uses
    /// this to decide whether to consult the next source in the
    /// fallback chain.
    pub const fn is_empty(&self) -> bool {
        self.contract_name.is_none() && self.protocol_tag.is_none() && !self.is_proxy
    }
}

/// Errors raised by an individual fetcher call. Per-address failures
/// don't short-circuit the orchestration loop — they're logged and
/// counted in [`LabelBackfillStats::fetch_errors`].
#[derive(Debug, Error)]
pub enum LabelFetchError {
    /// `reqwest`-side error: DNS, TLS, connection, body decode.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// HTTP response had a non-success status.
    #[error("non-success status: {0}")]
    Status(reqwest::StatusCode),
    /// JSON body didn't parse with `serde_json`.
    #[error("json parse error: {0}")]
    Parse(#[from] serde_json::Error),
    /// JSON parsed but the structure wasn't what we expected.
    #[error("unexpected response shape: {0}")]
    Shape(String),
    /// DB read/write failed during orchestration.
    #[error("db error: {0}")]
    Db(#[from] DatabaseError),
}

/// Counters returned by [`run_contract_label_backfill_incremental`].
#[derive(Debug, Default, Clone, Copy)]
pub struct LabelBackfillStats {
    /// Number of addresses considered this tick.
    pub addresses_examined: u64,
    /// Addresses where any source returned a non-empty label.
    pub upserted_with_label: u64,
    /// Addresses recorded as `source=none` (every source missed). Still
    /// upserted so subsequent ticks skip them.
    pub upserted_empty: u64,
    /// Addresses where one or more fetchers raised a non-fatal error
    /// (the orchestrator falls through to the next source).
    pub fetch_errors: u64,
}

/// Counters returned by [`run_function_signature_backfill_incremental`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SignatureBackfillStats {
    /// Selectors considered this tick.
    pub selectors_examined: u64,
    /// Selectors where `OpenChain` returned at least one signature.
    pub resolved: u64,
    /// Selectors recorded as misses (still upserted so subsequent ticks
    /// skip them).
    pub unresolved: u64,
    /// HTTP / parse errors during the batched lookups.
    pub fetch_errors: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// User-supplied config file controlling which external sources the
/// backfill consults. Loaded from a TOML file via
/// [`LabelBackfillConfig::from_path`]. All fields are optional; an
/// absent or empty file is equivalent to "Blockscout + Sourcify
/// enabled with default URLs, Etherscan disabled."
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LabelBackfillConfig {
    /// Etherscan v2 API key. When absent, Etherscan is skipped in the
    /// fallback chain (Blockscout + Sourcify still run).
    pub etherscan_api_key: Option<String>,
    /// Override the Blockscout v2 base URL. Defaults to mainnet.
    pub blockscout_base_url: Option<String>,
    /// Override the Sourcify server base URL.
    pub sourcify_base_url: Option<String>,
    /// Override the Etherscan v2 endpoint.
    pub etherscan_base_url: Option<String>,
    /// Chain id supplied to Etherscan v2 and Sourcify path. Defaults
    /// to 1 (Ethereum mainnet).
    pub chain_id: Option<u64>,
    /// Override the `OpenChain` v1 base URL.
    pub openchain_base_url: Option<String>,
}

impl LabelBackfillConfig {
    /// Parse a TOML file. Missing file → returns
    /// [`LabelBackfillConfig::default`] without erroring, so the
    /// operator can leave the path unset and still get the default
    /// (key-less) fallback chain.
    pub fn from_path(path: &Path) -> Result<Self, std::io::Error> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| std::io::Error::other(format!("toml parse: {e}")))
    }

    /// Resolved chain id (falls back to mainnet).
    pub const fn resolved_chain_id(&self) -> u64 {
        match self.chain_id {
            Some(id) => id,
            None => DEFAULT_CHAIN_ID,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP-backed fetchers
// ─────────────────────────────────────────────────────────────────────────────

/// Compositional fetcher that runs the Blockscout → Sourcify →
/// Etherscan fallback chain for one address. Constructed once per
/// process from a [`LabelBackfillConfig`] and shared across all
/// orchestration ticks. Holds an `Arc`-cloneable `reqwest::Client`
/// for connection reuse.
#[derive(Debug)]
pub struct ContractLabelFetcher {
    http: reqwest::Client,
    blockscout_base: String,
    sourcify_base: String,
    etherscan_base: String,
    etherscan_api_key: Option<String>,
    chain_id: u64,
    blockscout_last: Mutex<Option<Instant>>,
    sourcify_last: Mutex<Option<Instant>>,
    etherscan_last: Mutex<Option<Instant>>,
}

impl ContractLabelFetcher {
    /// Construct from a config. Etherscan is automatically enabled iff
    /// the config supplied an API key.
    pub fn new(config: &LabelBackfillConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("reth-research/contract-labels")
            .build()
            .expect("reqwest client construction is infallible with defaults");

        Self {
            http,
            blockscout_base: config
                .blockscout_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BLOCKSCOUT_BASE.to_string()),
            sourcify_base: config
                .sourcify_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_SOURCIFY_BASE.to_string()),
            etherscan_base: config
                .etherscan_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_ETHERSCAN_BASE.to_string()),
            etherscan_api_key: config.etherscan_api_key.clone(),
            chain_id: config.resolved_chain_id(),
            blockscout_last: Mutex::new(None),
            sourcify_last: Mutex::new(None),
            etherscan_last: Mutex::new(None),
        }
    }

    /// Walks Blockscout → Sourcify → Etherscan, returning the first
    /// non-empty label along with the source that produced it.
    /// Returns `(ContractLabel::default(), SOURCE_NONE)` when every
    /// rung missed.
    pub async fn fetch(&self, address: Address) -> (ContractLabel, &'static str) {
        // 1. Blockscout — only source with curated protocol tags.
        match self.fetch_blockscout(address).await {
            Ok(Some(label)) if !label.is_empty() => return (label, SOURCE_BLOCKSCOUT),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    target: "research::external_labels",
                    %address, source = SOURCE_BLOCKSCOUT, error = %e,
                    "label fetch failed; trying next source"
                );
            }
        }

        // 2. Sourcify — Solidity class name only, no protocol tag.
        match self.fetch_sourcify(address).await {
            Ok(Some(label)) if !label.is_empty() => return (label, SOURCE_SOURCIFY),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    target: "research::external_labels",
                    %address, source = SOURCE_SOURCIFY, error = %e,
                    "label fetch failed; trying next source"
                );
            }
        }

        // 3. Etherscan — only if an API key was configured.
        if self.etherscan_api_key.is_some() {
            match self.fetch_etherscan(address).await {
                Ok(Some(label)) if !label.is_empty() => return (label, SOURCE_ETHERSCAN),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "research::external_labels",
                        %address, source = SOURCE_ETHERSCAN, error = %e,
                        "label fetch failed; giving up"
                    );
                }
            }
        }

        (ContractLabel::default(), SOURCE_NONE)
    }

    async fn fetch_blockscout(
        &self,
        address: Address,
    ) -> Result<Option<ContractLabel>, LabelFetchError> {
        rate_limit(&self.blockscout_last, BLOCKSCOUT_MIN_INTERVAL).await;
        let url = format!("{}/addresses/{address:#x}", self.blockscout_base);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        // Blockscout returns 404 for unknown addresses — treat as
        // "miss", not an error.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(LabelFetchError::Status(status));
        }
        let body = resp.text().await?;
        Ok(parse_blockscout_address_response(&body))
    }

    async fn fetch_sourcify(
        &self,
        address: Address,
    ) -> Result<Option<ContractLabel>, LabelFetchError> {
        rate_limit(&self.sourcify_last, SOURCIFY_MIN_INTERVAL).await;
        // Sourcify's `files/any/{chain}/{address}` returns a bundle
        // containing metadata.json among other files when the address
        // is verified (full or partial match), and 404 otherwise.
        let url = format!("{}/files/any/{}/{address:#x}", self.sourcify_base, self.chain_id);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(LabelFetchError::Status(status));
        }
        let body = resp.text().await?;
        Ok(parse_sourcify_files_response(&body))
    }

    async fn fetch_etherscan(
        &self,
        address: Address,
    ) -> Result<Option<ContractLabel>, LabelFetchError> {
        rate_limit(&self.etherscan_last, ETHERSCAN_MIN_INTERVAL).await;
        let key = self.etherscan_api_key.as_deref().unwrap_or("");
        let resp = self
            .http
            .get(&self.etherscan_base)
            .query(&[
                ("chainid", self.chain_id.to_string().as_str()),
                ("module", "contract"),
                ("action", "getsourcecode"),
                ("address", format!("{address:#x}").as_str()),
                ("apikey", key),
            ])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LabelFetchError::Status(status));
        }
        let body = resp.text().await?;
        Ok(parse_etherscan_getsourcecode_response(&body))
    }
}

/// Fetcher for `OpenChain`'s signature-database v1 endpoint. Looks up
/// up to [`OPENCHAIN_BATCH_SIZE`] selectors per request.
#[derive(Debug)]
pub struct OpenChainFetcher {
    http: reqwest::Client,
    base: String,
    last_call: Mutex<Option<Instant>>,
}

impl OpenChainFetcher {
    /// Construct with a base URL (typically `DEFAULT_OPENCHAIN_BASE`).
    pub fn new(base_url: Option<&str>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("reth-research/function-signatures")
            .build()
            .expect("reqwest client construction is infallible with defaults");
        Self {
            http,
            base: base_url.unwrap_or(DEFAULT_OPENCHAIN_BASE).to_string(),
            last_call: Mutex::new(None),
        }
    }

    /// Look up a batch of selectors. The returned map contains an
    /// entry for every input selector — `Some(sig)` for resolved
    /// selectors, `None` for misses — so the orchestrator can persist
    /// negative-cache rows. Selectors must be exactly 4 bytes each;
    /// non-4-byte inputs are silently dropped.
    pub async fn lookup_batch(
        &self,
        selectors: &[Vec<u8>],
    ) -> Result<HashMap<Vec<u8>, Option<String>>, LabelFetchError> {
        let mut out: HashMap<Vec<u8>, Option<String>> = HashMap::new();
        for chunk in selectors.chunks(OPENCHAIN_BATCH_SIZE) {
            rate_limit(&self.last_call, OPENCHAIN_MIN_INTERVAL).await;

            let hex_csv = chunk
                .iter()
                .filter(|s| s.len() == 4)
                .map(|s| format!("0x{}", hex_of(s)))
                .collect::<Vec<_>>()
                .join(",");

            if hex_csv.is_empty() {
                continue;
            }

            let url = format!("{}/lookup", self.base);
            let resp = self.http.get(&url).query(&[("function", hex_csv.as_str())]).send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(LabelFetchError::Status(status));
            }
            let body = resp.text().await?;
            let parsed = parse_openchain_lookup_response(&body)?;

            // Initialise every input selector — keeps the negative cache
            // promise even for selectors OpenChain doesn't mention in
            // its response.
            for sel in chunk {
                if sel.len() == 4 {
                    out.entry(sel.clone()).or_insert(None);
                }
            }
            for (sel_hex, sig) in parsed {
                if let Some(bytes) = hex_to_bytes(&sel_hex) &&
                    bytes.len() == 4
                {
                    out.insert(bytes, sig);
                }
            }
        }
        Ok(out)
    }
}

async fn rate_limit(last: &Mutex<Option<Instant>>, min_interval: Duration) {
    let mut guard = last.lock().await;
    if let Some(prev) = *guard {
        let elapsed = prev.elapsed();
        if elapsed < min_interval {
            tokio::time::sleep(min_interval - elapsed).await;
        }
    }
    *guard = Some(Instant::now());
}

// ─────────────────────────────────────────────────────────────────────────────
// Response parsers (pure, unit-testable)
// ─────────────────────────────────────────────────────────────────────────────

/// Subset of the Blockscout v2 `/addresses/{addr}` response that we
/// care about. Blockscout's full payload is much larger — we
/// intentionally only deserialize the label-relevant fields.
#[derive(Debug, Deserialize)]
struct BlockscoutAddressResponse {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    is_contract: Option<bool>,
    #[serde(default)]
    public_tags: Vec<BlockscoutTag>,
    #[serde(default)]
    private_tags: Vec<BlockscoutTag>,
    #[serde(default)]
    watchlist_names: Vec<BlockscoutTag>,
    #[serde(default)]
    implementations: Vec<BlockscoutImplementation>,
    #[serde(default)]
    token: Option<BlockscoutToken>,
    #[serde(default)]
    is_verified: Option<bool>,
    #[serde(default)]
    proxy_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BlockscoutTag {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl BlockscoutTag {
    fn best(&self) -> Option<&str> {
        self.display_name.as_deref().or(self.label.as_deref()).or(self.name.as_deref())
    }
}

#[derive(Debug, Deserialize)]
struct BlockscoutImplementation {
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    address_hash: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BlockscoutToken {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
}

/// Parse a Blockscout v2 `/addresses/{addr}` response body. Public so
/// unit tests can exercise it without hitting the network.
pub fn parse_blockscout_address_response(body: &str) -> Option<ContractLabel> {
    let resp: BlockscoutAddressResponse = serde_json::from_str(body).ok()?;

    // Skip pure EOAs — nothing to label.
    if resp.is_contract == Some(false) && resp.implementations.is_empty() && resp.token.is_none() {
        return None;
    }

    // Solidity class name. Prefer the address-level `name` over the
    // first implementation's `name`.
    let contract_name =
        resp.name.clone().or_else(|| resp.implementations.iter().find_map(|imp| imp.name.clone()));

    // Curated protocol tag — try public_tags first, then watchlist /
    // private. Pick the first label that fits; Blockscout doesn't rank
    // them and they're rarely conflicting.
    let curated_tag = resp
        .public_tags
        .iter()
        .find_map(BlockscoutTag::best)
        .or_else(|| resp.watchlist_names.iter().find_map(BlockscoutTag::best))
        .or_else(|| resp.private_tags.iter().find_map(BlockscoutTag::best))
        .map(str::to_string);

    // Fall back to "<TokenName> (<symbol>)" when no other tag fired
    // and the address is an ERC-20 — many proxies and direct token
    // contracts only surface through the `token` field.
    let protocol_tag = curated_tag.or_else(|| {
        let tok = resp.token.as_ref()?;
        let name = tok.name.as_ref()?;
        if let Some(sym) = tok.symbol.as_ref() {
            Some(format!("{name} ({sym})"))
        } else {
            Some(name.clone())
        }
    });

    // Proxy detection. `proxy_type` is a recent Blockscout addition
    // (e.g. "eip1967", "eip897"); the legacy signal is a non-empty
    // `implementations` list.
    let proxy_via_field =
        resp.proxy_type.as_deref().is_some_and(|t| !t.is_empty() && t != "unknown");
    let proxy_via_impls = !resp.implementations.is_empty();
    let is_proxy = proxy_via_field || proxy_via_impls;
    let impl_address = resp.implementations.iter().find_map(|imp| {
        let s = imp.address.as_deref().or(imp.address_hash.as_deref())?;
        s.parse::<Address>().ok()
    });

    let _ = resp.is_verified; // Field captured for completeness; not used.

    let label = ContractLabel { contract_name, protocol_tag, is_proxy, impl_address };
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

/// Subset of Sourcify's `/files/any/{chain}/{addr}` response. Sourcify
/// returns a JSON object with `status` and a `files` array; we want
/// the `metadata.json` entry's parsed `compilationTarget`.
#[derive(Debug, Deserialize)]
struct SourcifyFilesResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    files: Vec<SourcifyFile>,
}

#[derive(Debug, Deserialize)]
struct SourcifyFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// Parse a Sourcify response. Public for unit testing.
pub fn parse_sourcify_files_response(body: &str) -> Option<ContractLabel> {
    let resp: SourcifyFilesResponse = serde_json::from_str(body).ok()?;

    let metadata = resp.files.iter().find(|f| {
        f.name.as_deref() == Some("metadata.json") ||
            f.path.as_deref().is_some_and(|p| p.ends_with("metadata.json"))
    })?;
    let content = metadata.content.as_deref()?;

    // metadata.json has `settings.compilationTarget = { "Foo.sol": "Foo" }`.
    let meta: serde_json::Value = serde_json::from_str(content).ok()?;
    let target = meta.get("settings")?.get("compilationTarget")?.as_object()?;
    let contract_name = target.values().next()?.as_str()?.to_string();

    // Sourcify doesn't classify proxies; `status` carries the
    // full/partial match flavor but we treat both as valid labels.
    let _ = resp.status;
    let label = ContractLabel { contract_name: Some(contract_name), ..Default::default() };
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

/// Subset of the Etherscan v2 `getsourcecode` response. Etherscan
/// wraps the result in a status envelope; the inner array always has
/// exactly one element for a valid call.
#[derive(Debug, Deserialize)]
struct EtherscanGetSourceCodeResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    result: Vec<EtherscanContractResult>,
}

#[derive(Debug, Deserialize)]
struct EtherscanContractResult {
    #[serde(default, rename = "ContractName")]
    contract_name: Option<String>,
    #[serde(default, rename = "Implementation")]
    implementation: Option<String>,
    #[serde(default, rename = "Proxy")]
    proxy: Option<String>,
}

/// Parse an Etherscan v2 `getsourcecode` response. Public for unit
/// testing.
pub fn parse_etherscan_getsourcecode_response(body: &str) -> Option<ContractLabel> {
    let resp: EtherscanGetSourceCodeResponse = serde_json::from_str(body).ok()?;
    // Etherscan's `status` is "1" on success; "0" means rate-limited
    // or invalid address.
    if resp.status.as_deref() == Some("0") {
        return None;
    }
    let first = resp.result.first()?;
    let name =
        first.contract_name.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let is_proxy = first.proxy.as_deref() == Some("1");
    let impl_address = first
        .implementation
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "0x0000000000000000000000000000000000000000")
        .and_then(|s| s.parse::<Address>().ok());

    let label = ContractLabel { contract_name: name, protocol_tag: None, is_proxy, impl_address };
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

/// Subset of the `OpenChain` signature-database response.
#[derive(Debug, Deserialize)]
struct OpenChainLookupResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Option<OpenChainResult>,
}

#[derive(Debug, Deserialize)]
struct OpenChainResult {
    #[serde(default)]
    function: HashMap<String, Option<Vec<OpenChainEntry>>>,
}

#[derive(Debug, Deserialize)]
struct OpenChainEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    filtered: Option<bool>,
}

/// Parse an `OpenChain` lookup response into a `selector_hex →
/// Option<signature>` map. Picks the first non-filtered entry per
/// selector. Public for unit testing.
pub fn parse_openchain_lookup_response(
    body: &str,
) -> Result<HashMap<String, Option<String>>, LabelFetchError> {
    let resp: OpenChainLookupResponse =
        serde_json::from_str(body).map_err(LabelFetchError::Parse)?;
    if !resp.ok {
        return Err(LabelFetchError::Shape("ok=false".into()));
    }
    let Some(result) = resp.result else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    for (hex, entries) in result.function {
        let sig = entries.and_then(|list| {
            list.into_iter().find(|e| !e.filtered.unwrap_or(false)).and_then(|e| e.name)
        });
        out.insert(hex, sig);
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Orchestration
// ─────────────────────────────────────────────────────────────────────────────

/// Walks the unlabeled address set once, consulting Blockscout →
/// Sourcify → Etherscan for each, and UPSERTs a `contract_labels`
/// row (including negative-cache rows for total misses).
pub async fn run_contract_label_backfill_incremental(
    db: &DivergenceDatabase,
    fetcher: &ContractLabelFetcher,
) -> Result<LabelBackfillStats, LabelFetchError> {
    let addresses = db.distinct_unlabeled_addresses_for_labels()?;
    let extracted_at =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut stats = LabelBackfillStats::default();

    for addr_str in addresses {
        stats.addresses_examined += 1;
        let Ok(address) = addr_str.parse::<Address>() else {
            tracing::warn!(
                target: "research::external_labels",
                address = %addr_str,
                "skipping malformed address from divergence_call_frames"
            );
            stats.fetch_errors += 1;
            continue;
        };

        let (label, source) = fetcher.fetch(address).await;
        let impl_address_str = label.impl_address.map(|a| format!("{a:#x}"));

        let res = db.upsert_contract_label(
            &addr_str,
            label.contract_name.as_deref(),
            label.protocol_tag.as_deref(),
            label.is_proxy,
            impl_address_str.as_deref(),
            source,
            extracted_at,
        );
        if let Err(e) = res {
            return Err(LabelFetchError::Db(e));
        }
        if source == SOURCE_NONE {
            stats.upserted_empty += 1;
        } else {
            stats.upserted_with_label += 1;
        }
    }
    Ok(stats)
}

/// Walks the unresolved-selector set once, queries `OpenChain` in
/// batches, and UPSERTs a `function_signatures` row per selector
/// (including misses as negative-cache).
pub async fn run_function_signature_backfill_incremental(
    db: &DivergenceDatabase,
    fetcher: &OpenChainFetcher,
) -> Result<SignatureBackfillStats, LabelFetchError> {
    let selectors = db.distinct_unresolved_selectors()?;
    let extracted_at =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut stats =
        SignatureBackfillStats { selectors_examined: selectors.len() as u64, ..Default::default() };

    if selectors.is_empty() {
        return Ok(stats);
    }

    let lookups = match fetcher.lookup_batch(&selectors).await {
        Ok(map) => map,
        Err(e) => {
            stats.fetch_errors += 1;
            tracing::warn!(
                target: "research::external_labels",
                error = %e,
                "openchain batch lookup failed; counting as fetch_error"
            );
            return Ok(stats);
        }
    };

    for selector in &selectors {
        let sig = lookups.get(selector).cloned().unwrap_or(None);
        let source = if sig.is_some() { SOURCE_OPENCHAIN } else { SOURCE_NONE };
        let res = db.upsert_function_signature(selector, sig.as_deref(), source, extracted_at);
        if let Err(e) = res {
            return Err(LabelFetchError::Db(e));
        }
        if sig.is_some() {
            stats.resolved += 1;
        } else {
            stats.unresolved += 1;
        }
    }
    Ok(stats)
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    if !stripped.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(stripped.len() / 2);
    for i in (0..stripped.len()).step_by(2) {
        out.push(u8::from_str_radix(&stripped[i..i + 2], 16).ok()?);
    }
    Some(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blockscout_parser_extracts_protocol_tag_and_proxy() {
        let body = r#"{
            "name": "FiatTokenProxy",
            "is_contract": true,
            "is_verified": true,
            "public_tags": [{"display_name": "USDC"}],
            "private_tags": [],
            "watchlist_names": [],
            "implementations": [{
                "address": "0x43506849D7C04F9138D1A2050bbF3A0c054402dd",
                "name": "FiatTokenV2_2"
            }],
            "token": {"name": "USD Coin", "symbol": "USDC"},
            "proxy_type": "eip1967"
        }"#;
        let label = parse_blockscout_address_response(body).unwrap();
        assert_eq!(label.protocol_tag.as_deref(), Some("USDC"));
        assert_eq!(label.contract_name.as_deref(), Some("FiatTokenProxy"));
        assert!(label.is_proxy);
        assert!(label.impl_address.is_some());
    }

    #[test]
    fn blockscout_parser_uses_token_when_no_tag() {
        let body = r#"{
            "name": null,
            "is_contract": true,
            "public_tags": [],
            "private_tags": [],
            "watchlist_names": [],
            "implementations": [],
            "token": {"name": "USD Coin", "symbol": "USDC"}
        }"#;
        let label = parse_blockscout_address_response(body).unwrap();
        assert_eq!(label.protocol_tag.as_deref(), Some("USD Coin (USDC)"));
        assert!(!label.is_proxy);
    }

    #[test]
    fn blockscout_parser_returns_none_for_pure_eoa() {
        let body = r#"{
            "is_contract": false,
            "public_tags": [],
            "private_tags": [],
            "watchlist_names": [],
            "implementations": []
        }"#;
        assert!(parse_blockscout_address_response(body).is_none());
    }

    #[test]
    fn sourcify_parser_extracts_solidity_class_name() {
        let body = r#"{
            "status": "full",
            "files": [
                {
                    "name": "metadata.json",
                    "path": "/x/0xabc/metadata.json",
                    "content": "{\"settings\":{\"compilationTarget\":{\"contracts/USDC.sol\":\"FiatTokenV2_2\"}}}"
                },
                {"name": "USDC.sol", "content": "// source"}
            ]
        }"#;
        let label = parse_sourcify_files_response(body).unwrap();
        assert_eq!(label.contract_name.as_deref(), Some("FiatTokenV2_2"));
        assert!(label.protocol_tag.is_none());
        assert!(!label.is_proxy);
    }

    #[test]
    fn sourcify_parser_returns_none_when_no_metadata_file() {
        let body = r#"{"status": "partial", "files": [{"name": "USDC.sol", "content": "// src"}]}"#;
        assert!(parse_sourcify_files_response(body).is_none());
    }

    #[test]
    fn etherscan_parser_extracts_contract_name_and_proxy() {
        let body = r#"{
            "status": "1",
            "message": "OK",
            "result": [{
                "ContractName": "FiatTokenProxy",
                "Proxy": "1",
                "Implementation": "0x43506849d7c04f9138d1a2050bbf3a0c054402dd",
                "ABI": "..."
            }]
        }"#;
        let label = parse_etherscan_getsourcecode_response(body).unwrap();
        assert_eq!(label.contract_name.as_deref(), Some("FiatTokenProxy"));
        assert!(label.is_proxy);
        assert!(label.impl_address.is_some());
    }

    #[test]
    fn etherscan_parser_returns_none_on_status_zero() {
        let body = r#"{"status":"0","message":"NOTOK","result":"Invalid API Key"}"#;
        // status=0 with non-array result will fail to deserialize; we
        // also handle status=0 with an empty array.
        let body2 = r#"{"status":"0","message":"NOTOK","result":[]}"#;
        assert!(parse_etherscan_getsourcecode_response(body).is_none());
        assert!(parse_etherscan_getsourcecode_response(body2).is_none());
    }

    #[test]
    fn etherscan_parser_handles_unverified_contract() {
        // For unverified contracts Etherscan still returns status=1
        // with an empty ContractName.
        let body = r#"{
            "status": "1",
            "message": "OK",
            "result": [{
                "ContractName": "",
                "Proxy": "0",
                "Implementation": "",
                "ABI": "Contract source code not verified"
            }]
        }"#;
        assert!(parse_etherscan_getsourcecode_response(body).is_none());
    }

    #[test]
    fn openchain_parser_picks_non_filtered_signature() {
        let body = r#"{
            "ok": true,
            "result": {
                "function": {
                    "0xa9059cbb": [
                        {"name": "transfer(address,uint256)", "filtered": false}
                    ],
                    "0xdeadbeef": null
                }
            }
        }"#;
        let parsed = parse_openchain_lookup_response(body).unwrap();
        assert_eq!(parsed.get("0xa9059cbb"), Some(&Some("transfer(address,uint256)".to_string())));
        assert_eq!(parsed.get("0xdeadbeef"), Some(&None));
    }

    #[test]
    fn openchain_parser_skips_filtered_entries() {
        let body = r#"{
            "ok": true,
            "result": {
                "function": {
                    "0x12345678": [
                        {"name": "spam(uint256)", "filtered": true},
                        {"name": "real_function(uint256)", "filtered": false}
                    ]
                }
            }
        }"#;
        let parsed = parse_openchain_lookup_response(body).unwrap();
        assert_eq!(parsed.get("0x12345678"), Some(&Some("real_function(uint256)".to_string())));
    }

    #[test]
    fn openchain_parser_rejects_ok_false() {
        let body = r#"{"ok": false, "result": null}"#;
        assert!(parse_openchain_lookup_response(body).is_err());
    }

    #[test]
    fn hex_to_bytes_roundtrip() {
        let bytes = vec![0xa9, 0x05, 0x9c, 0xbb];
        assert_eq!(hex_of(&bytes), "a9059cbb");
        assert_eq!(hex_to_bytes("0xa9059cbb").as_deref(), Some(bytes.as_slice()));
        assert_eq!(hex_to_bytes("a9059cbb").as_deref(), Some(bytes.as_slice()));
        assert!(hex_to_bytes("xyz").is_none());
    }

    #[test]
    fn label_backfill_config_default_when_missing() {
        let path = std::path::PathBuf::from("/nonexistent/path/to/config.toml");
        let cfg = LabelBackfillConfig::from_path(&path).unwrap();
        assert!(cfg.etherscan_api_key.is_none());
        assert_eq!(cfg.resolved_chain_id(), 1);
    }

    #[test]
    fn label_backfill_config_parses_toml() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "etherscan_api_key = \"deadbeef\"\nchain_id = 11155111\n")
            .unwrap();
        let cfg = LabelBackfillConfig::from_path(tmp.path()).unwrap();
        assert_eq!(cfg.etherscan_api_key.as_deref(), Some("deadbeef"));
        assert_eq!(cfg.resolved_chain_id(), 11155111);
    }
}
