//! Solidity metadata-trailer parser for the `contract_metadata` sidecar table.
//!
//! Modern Solidity contracts end with a CBOR-encoded metadata blob followed by
//! a 2-byte big-endian length. The trailer commonly carries:
//!
//! - `solc`: 3-byte semver (major, minor, patch)
//! - `ipfs` or `bzzr1` / `bzzr0`: metadata-document hash (multihash bytes)
//! - `experimental`: boolean (older versions; rare today)
//!
//! See <https://docs.soliditylang.org/en/latest/metadata.html> for the spec.
//!
//! This module ships a minimal hand-rolled CBOR decoder rather than pulling in
//! a full crate — the trailer's shape is narrow (small maps, short byte / text
//! strings, plus the occasional bool) and parsing it is on the cold path
//! (`contract-metadata-backfill` subcommand, run out-of-band against the
//! producer DB).

use crate::database::{DatabaseError, DivergenceDatabase};
use alloy_primitives::{keccak256, Address};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Anything that can fail when locating or decoding the CBOR trailer.
#[derive(Debug, Error)]
pub enum MetadataParseError {
    /// Bytecode is shorter than the 2-byte length prefix.
    #[error("bytecode too short for metadata trailer: {0} bytes")]
    BytecodeTooShort(usize),
    /// The length prefix points outside the bytecode range.
    #[error("metadata length {len} doesn't fit in {bytecode_len} bytes of bytecode")]
    LengthOutOfRange {
        /// The (oversized) length the trailing 2-byte prefix declared.
        len: usize,
        /// The bytecode's actual length, in bytes.
        bytecode_len: usize,
    },
    /// CBOR decode failure (truncated input, unsupported type, etc.).
    #[error("CBOR decode failed: {0}")]
    CborDecode(&'static str),
}

/// Decoded contents of a Solidity metadata trailer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractMetadata {
    /// `"M.m.p"` extracted from the `solc` 3-byte version field. `None` when
    /// the key is absent.
    pub solc_version: Option<String>,
    /// Optional `solc` commit hash. Solidity doesn't currently emit this
    /// inside the CBOR (commit is in the trailing build-info JSON), so we
    /// keep the column for future use but always set it to `None` from a
    /// pure bytecode read.
    pub solc_commit: Option<String>,
    /// Heuristic EVM target — currently always `None` because Solidity
    /// doesn't encode the EVM target in CBOR. Left as a column for the
    /// future (e.g. inferring from solc version + compile-time defaults).
    pub evm_target: Option<String>,
    /// `true` iff the trailer was successfully parsed as CBOR.
    pub cbor_present: bool,
    /// `true` iff the trailer carried an `ipfs` or `bzzr*` hash key.
    pub has_metadata_hash: bool,
}

/// Parse a contract's bytecode for the Solidity CBOR metadata trailer.
///
/// `bytecode` should be the contract's deployed code (the result of
/// `eth_getCode`). Returns the decoded metadata, or `Ok(ContractMetadata
/// { cbor_present: false, .. })` if no recognisable trailer is present —
/// some hand-written / very old contracts have no trailer at all, and
/// that's a successful read, not a parse error.
pub fn parse_metadata_trailer(bytecode: &[u8]) -> Result<ContractMetadata, MetadataParseError> {
    if bytecode.len() < 2 {
        return Err(MetadataParseError::BytecodeTooShort(bytecode.len()));
    }
    let len =
        u16::from_be_bytes([bytecode[bytecode.len() - 2], bytecode[bytecode.len() - 1]]) as usize;
    if len == 0 || len + 2 > bytecode.len() {
        // No trailer / impossibly-long length → treat as "no metadata"
        // rather than a parse error.
        return Ok(ContractMetadata::default());
    }
    let cbor_start = bytecode.len() - 2 - len;
    let cbor = &bytecode[cbor_start..bytecode.len() - 2];

    match decode_map(cbor) {
        Ok(meta) => Ok(meta),
        Err(_) => Ok(ContractMetadata::default()),
    }
}

/// Decode a CBOR map of the small shape Solidity emits. Only handles the
/// subset of CBOR we know the trailer uses: text-string keys, byte-string
/// or bool values, fixed-size maps with up to 23 entries.
fn decode_map(buf: &[u8]) -> Result<ContractMetadata, MetadataParseError> {
    let mut cur = Cursor::new(buf);
    let initial = cur.next()?;
    let major = initial >> 5;
    if major != 5 {
        return Err(MetadataParseError::CborDecode("expected map at root"));
    }
    let entries = (initial & 0x1f) as usize;
    if entries > 23 {
        return Err(MetadataParseError::CborDecode("map with > 23 entries unsupported"));
    }

    let mut meta = ContractMetadata { cbor_present: true, ..ContractMetadata::default() };

    for _ in 0..entries {
        let key = cur.read_text_string()?;
        match key.as_str() {
            "solc" => {
                let bytes = cur.read_byte_string()?;
                if bytes.len() == 3 {
                    meta.solc_version = Some(format!("{}.{}.{}", bytes[0], bytes[1], bytes[2]));
                } else {
                    // Unknown shape — skip without bailing on the whole trailer.
                }
            }
            "ipfs" | "bzzr0" | "bzzr1" | "ipfs1" => {
                let _ = cur.read_byte_string()?;
                meta.has_metadata_hash = true;
            }
            "experimental" => {
                cur.read_bool()?;
            }
            _ => {
                // Unknown key — skip its value without trying to interpret.
                cur.skip_value()?;
            }
        }
    }

    Ok(meta)
}

/// Minimal cursor over a CBOR byte slice. Only implements the variants the
/// Solidity trailer uses.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    const fn next(&mut self) -> Result<u8, MetadataParseError> {
        if self.pos >= self.buf.len() {
            return Err(MetadataParseError::CborDecode("truncated"));
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Read a length argument from a major-type initial byte. Handles
    /// short-form (lower 5 bits) and the 1/2/4/8-byte extended forms.
    fn read_arg(&mut self, initial: u8) -> Result<u64, MetadataParseError> {
        let info = initial & 0x1f;
        match info {
            0..=23 => Ok(info as u64),
            24 => self.next().map(|b| b as u64),
            25 => {
                let hi = self.next()? as u64;
                let lo = self.next()? as u64;
                Ok((hi << 8) | lo)
            }
            26 => {
                let mut v = 0u64;
                for _ in 0..4 {
                    v = (v << 8) | self.next()? as u64;
                }
                Ok(v)
            }
            27 => {
                let mut v = 0u64;
                for _ in 0..8 {
                    v = (v << 8) | self.next()? as u64;
                }
                Ok(v)
            }
            _ => Err(MetadataParseError::CborDecode("unsupported length form")),
        }
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], MetadataParseError> {
        if self.pos + n > self.buf.len() {
            return Err(MetadataParseError::CborDecode("truncated bytes"));
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_text_string(&mut self) -> Result<String, MetadataParseError> {
        let initial = self.next()?;
        let major = initial >> 5;
        if major != 3 {
            return Err(MetadataParseError::CborDecode("expected text string"));
        }
        let len = self.read_arg(initial)? as usize;
        let bytes = self.read_bytes(len)?;
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|_| MetadataParseError::CborDecode("invalid UTF-8 in text string"))
    }

    fn read_byte_string(&mut self) -> Result<Vec<u8>, MetadataParseError> {
        let initial = self.next()?;
        let major = initial >> 5;
        if major != 2 {
            return Err(MetadataParseError::CborDecode("expected byte string"));
        }
        let len = self.read_arg(initial)? as usize;
        Ok(self.read_bytes(len)?.to_vec())
    }

    fn read_bool(&mut self) -> Result<bool, MetadataParseError> {
        let initial = self.next()?;
        match initial {
            0xf4 => Ok(false),
            0xf5 => Ok(true),
            _ => Err(MetadataParseError::CborDecode("expected bool")),
        }
    }

    /// Skip a value of any (supported) CBOR major type. Used when we hit
    /// an unknown map key whose value we don't care about.
    fn skip_value(&mut self) -> Result<(), MetadataParseError> {
        let initial = self.next()?;
        let major = initial >> 5;
        match major {
            // unsigned int / negative int: arg is the value itself
            0 | 1 => {
                self.read_arg(initial)?;
            }
            // byte string / text string: arg is the length
            2 | 3 => {
                let len = self.read_arg(initial)? as usize;
                self.read_bytes(len)?;
            }
            // array: arg is the count
            4 => {
                let count = self.read_arg(initial)? as usize;
                for _ in 0..count {
                    self.skip_value()?;
                }
            }
            // map: arg is pair count
            5 => {
                let pairs = self.read_arg(initial)? as usize;
                for _ in 0..pairs {
                    self.skip_value()?;
                    self.skip_value()?;
                }
            }
            // tagged: read the tag, then the wrapped value
            6 => {
                self.read_arg(initial)?;
                self.skip_value()?;
            }
            // simple / float / break: only the no-content variants are
            // expected for Solidity (true/false/null).
            7 => match initial {
                0xf4..=0xf7 => {}
                _ => return Err(MetadataParseError::CborDecode("unsupported simple value")),
            },
            _ => return Err(MetadataParseError::CborDecode("unsupported major type")),
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backfill orchestration
// ─────────────────────────────────────────────────────────────────────────────

/// Abstraction over "fetch deployed bytecode for an address". Implemented
/// against reth's `StateProvider` in the bin crate via a one-line
/// adapter; the orchestration loop here stays generic so the work is
/// testable with a synthetic fetcher.
pub trait BytecodeFetcher {
    /// Return the deployed bytecode for `address`, or `Ok(None)` if the
    /// account is empty / self-destructed / never had code.
    fn fetch_bytecode(&self, address: Address) -> Result<Option<Vec<u8>>, BackfillError>;
}

/// Errors raised by the metadata-backfill orchestration loop. Failures
/// are scoped — a fetch failure on one address logs and continues; only
/// a DB write error or address-parse error short-circuits.
#[derive(Debug, Error)]
pub enum BackfillError {
    /// `SQLite` read/write failed.
    #[error("database error: {0}")]
    Db(#[from] DatabaseError),
    /// `to_address` column held a value that didn't parse as 0x-prefixed
    /// hex. Indicates a producer-side bug since the producer always
    /// formats addresses with `{addr:#x}`.
    #[error("malformed address in divergence_call_frames: {0}")]
    AddressParse(String),
    /// Surface area for fetcher-specific errors (state lookup failures,
    /// missing reth datadir, etc.).
    #[error("bytecode fetch failed for {address}: {source}")]
    Fetch {
        /// The address whose bytecode we tried to fetch.
        address: Address,
        /// Underlying fetcher error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Counters returned by [`run_metadata_backfill`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BackfillStats {
    /// Number of distinct addresses considered.
    pub addresses_examined: u64,
    /// Addresses where `contract_metadata` was `UPSERTed` (new codehash).
    pub upserted: u64,
    /// Addresses skipped because their codehash already had a row.
    pub skipped_existing: u64,
    /// Addresses whose bytecode lookup returned `None` (EOA, self-destructed,
    /// never deployed). Counted but not an error.
    pub no_bytecode: u64,
    /// Addresses where the bytecode fetch itself raised an error (the
    /// loop logs and continues).
    pub fetch_errors: u64,
}

/// Iterate every distinct `to_address` in `divergence_call_frames`, ask
/// the fetcher for its bytecode, hash it to get a codehash, parse the
/// metadata trailer, and UPSERT into `contract_metadata`.
///
/// Idempotent and resumable: a codehash that already has a row is
/// skipped without re-fetching. Failures on individual addresses are
/// logged via `tracing` and counted in [`BackfillStats::fetch_errors`].
///
/// Aborts only on DB errors or malformed-address strings — both of
/// which indicate a producer-side bug rather than an external failure.
pub fn run_metadata_backfill(
    db: &DivergenceDatabase,
    fetcher: &dyn BytecodeFetcher,
) -> Result<BackfillStats, BackfillError> {
    let addresses = db.distinct_call_frame_addresses()?;
    run_backfill_for_addresses(db, fetcher, addresses)
}

/// Like [`run_metadata_backfill`] but pre-filters at the SQL layer to
/// addresses whose historical codehash isn't yet in `contract_metadata`
/// (or whose codehash is NULL). Suitable for periodic background ticks
/// where re-scanning every address each pass would issue redundant
/// state lookups as the DB grows.
pub fn run_metadata_backfill_incremental(
    db: &DivergenceDatabase,
    fetcher: &dyn BytecodeFetcher,
) -> Result<BackfillStats, BackfillError> {
    let addresses = db.distinct_unlabeled_call_frame_addresses()?;
    run_backfill_for_addresses(db, fetcher, addresses)
}

fn run_backfill_for_addresses(
    db: &DivergenceDatabase,
    fetcher: &dyn BytecodeFetcher,
    addresses: Vec<String>,
) -> Result<BackfillStats, BackfillError> {
    let extracted_at =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut stats = BackfillStats::default();
    for addr_str in addresses {
        stats.addresses_examined += 1;
        let address: Address =
            addr_str.parse().map_err(|_| BackfillError::AddressParse(addr_str.clone()))?;

        let bytecode = match fetcher.fetch_bytecode(address) {
            Ok(Some(bc)) => bc,
            Ok(None) => {
                stats.no_bytecode += 1;
                tracing::debug!(
                    target: "research::contract_metadata",
                    %address,
                    "no bytecode at address; skipping"
                );
                continue;
            }
            Err(e) => {
                stats.fetch_errors += 1;
                tracing::warn!(
                    target: "research::contract_metadata",
                    %address,
                    error = %e,
                    "bytecode fetch failed; skipping"
                );
                continue;
            }
        };

        let codehash = keccak256(&bytecode);
        let codehash_bytes: [u8; 32] = codehash.0;
        if db.has_contract_metadata(codehash_bytes)? {
            stats.skipped_existing += 1;
            continue;
        }

        let metadata = parse_metadata_trailer(&bytecode).unwrap_or_default();
        db.upsert_contract_metadata(
            codehash_bytes,
            &addr_str,
            bytecode.len() as u64,
            &metadata,
            extracted_at,
        )?;
        stats.upserted += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-crafted CBOR trailer:
    ///   {"ipfs": <34 bytes>, "solc": [0x00, 0x08, 0x15]}  (Solidity 0.8.21)
    /// followed by the 2-byte big-endian length.
    fn solc_0_8_21_trailer() -> Vec<u8> {
        let mut cbor = Vec::new();
        cbor.push(0xa2); // map of 2

        // "ipfs": byte-string of 34 bytes
        cbor.push(0x64);
        cbor.extend_from_slice(b"ipfs");
        cbor.push(0x58); // byte string, 1-byte length follows
        cbor.push(34);
        cbor.extend_from_slice(&[0u8; 34]);

        // "solc": byte-string of 3 bytes
        cbor.push(0x64);
        cbor.extend_from_slice(b"solc");
        cbor.push(0x43); // byte string of length 3
        cbor.push(0x00);
        cbor.push(0x08);
        cbor.push(0x15);

        let len = cbor.len() as u16;
        let mut trailer = cbor;
        trailer.extend_from_slice(&len.to_be_bytes());
        trailer
    }

    fn bzzr1_trailer() -> Vec<u8> {
        let mut cbor = Vec::new();
        cbor.push(0xa2); // map of 2
        cbor.push(0x65); // text string of length 5
        cbor.extend_from_slice(b"bzzr1");
        cbor.push(0x58);
        cbor.push(32);
        cbor.extend_from_slice(&[0u8; 32]);
        cbor.push(0x64);
        cbor.extend_from_slice(b"solc");
        cbor.push(0x43);
        cbor.push(0x00);
        cbor.push(0x05);
        cbor.push(0x10);

        let len = cbor.len() as u16;
        let mut trailer = cbor;
        trailer.extend_from_slice(&len.to_be_bytes());
        trailer
    }

    #[test]
    fn parse_solc_0_8_21_trailer() {
        // Synthetic bytecode: opcodes + CBOR trailer.
        let mut bytecode = vec![0x60, 0x80, 0x60, 0x40, 0x52];
        bytecode.extend_from_slice(&solc_0_8_21_trailer());

        let meta = parse_metadata_trailer(&bytecode).unwrap();
        assert!(meta.cbor_present);
        assert!(meta.has_metadata_hash);
        assert_eq!(meta.solc_version.as_deref(), Some("0.8.21"));
    }

    #[test]
    fn parse_bzzr1_trailer() {
        let mut bytecode = vec![0x60, 0x80];
        bytecode.extend_from_slice(&bzzr1_trailer());

        let meta = parse_metadata_trailer(&bytecode).unwrap();
        assert!(meta.cbor_present);
        assert!(meta.has_metadata_hash);
        assert_eq!(meta.solc_version.as_deref(), Some("0.5.16"));
    }

    #[test]
    fn parse_handles_experimental_flag() {
        // Map of 3: ipfs, solc, experimental=true
        let mut cbor = Vec::new();
        cbor.push(0xa3);
        cbor.push(0x64);
        cbor.extend_from_slice(b"ipfs");
        cbor.push(0x58);
        cbor.push(34);
        cbor.extend_from_slice(&[0u8; 34]);
        cbor.push(0x64);
        cbor.extend_from_slice(b"solc");
        cbor.push(0x43);
        cbor.push(0x00);
        cbor.push(0x05);
        cbor.push(0x0e);
        cbor.push(0x6c); // "experimental" is 12 bytes
        cbor.extend_from_slice(b"experimental");
        cbor.push(0xf5); // bool true

        let len = cbor.len() as u16;
        let mut bytecode = vec![0x00];
        bytecode.extend_from_slice(&cbor);
        bytecode.extend_from_slice(&len.to_be_bytes());

        let meta = parse_metadata_trailer(&bytecode).unwrap();
        assert!(meta.cbor_present);
        assert_eq!(meta.solc_version.as_deref(), Some("0.5.14"));
    }

    #[test]
    fn parse_returns_default_for_no_trailer() {
        // Bytecode with last 2 bytes that don't point to a CBOR map.
        let bytecode = vec![0x60, 0x80, 0x60, 0x40, 0x52, 0xff, 0xff];
        let meta = parse_metadata_trailer(&bytecode).unwrap();
        // Last-2-bytes length is 0xffff, far longer than 7 bytes of code → no trailer.
        assert!(!meta.cbor_present);
        assert!(!meta.has_metadata_hash);
        assert!(meta.solc_version.is_none());
    }

    #[test]
    fn parse_returns_default_when_cbor_decode_fails() {
        // Length 4 (matches buf), but the 4 bytes don't decode as a CBOR map.
        let bytecode = vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x04];
        let meta = parse_metadata_trailer(&bytecode).unwrap();
        assert!(!meta.cbor_present);
    }

    #[test]
    fn parse_errors_on_too_short_bytecode() {
        let res = parse_metadata_trailer(&[0xff]);
        assert!(matches!(res, Err(MetadataParseError::BytecodeTooShort(1))));
    }

    use crate::database::{
        BlockCoverageRow, BlockOutput, CallFrameRow, DivergenceRow, DrillInRecord,
    };
    use alloy_primitives::B256;
    use std::{cell::RefCell, collections::HashMap};

    /// Synthetic fetcher backed by a `HashMap`. The orchestration test
    /// drives the same code path the real reth-state adapter will exercise
    /// in production.
    struct MockFetcher {
        bytecodes: RefCell<HashMap<Address, Option<Vec<u8>>>>,
        fail: RefCell<HashMap<Address, &'static str>>,
    }

    impl MockFetcher {
        fn new() -> Self {
            Self { bytecodes: RefCell::new(HashMap::new()), fail: RefCell::new(HashMap::new()) }
        }

        fn set(&self, addr: Address, code: Option<Vec<u8>>) {
            self.bytecodes.borrow_mut().insert(addr, code);
        }

        fn set_failure(&self, addr: Address, msg: &'static str) {
            self.fail.borrow_mut().insert(addr, msg);
        }
    }

    impl BytecodeFetcher for MockFetcher {
        fn fetch_bytecode(&self, address: Address) -> Result<Option<Vec<u8>>, BackfillError> {
            if let Some(msg) = self.fail.borrow().get(&address).copied() {
                return Err(BackfillError::Fetch { address, source: msg.into() });
            }
            Ok(self.bytecodes.borrow().get(&address).cloned().flatten())
        }
    }

    fn seed_drill_in(db: &DivergenceDatabase, tx_index: u32, addr: Address) {
        let drill_in = DrillInRecord {
            divergence: DivergenceRow {
                schedule_name: "test".to_string(),
                schedule_config_hash: "cfg".to_string(),
                block_number: 1,
                tx_index,
                recipient: Some(addr),
                baseline_success: true,
                schedule_success: false,
                status_changed: true,
                ..Default::default()
            },
            call_frames: vec![CallFrameRow {
                call_index: 0,
                parent_call_index: None,
                depth: 0,
                from_address: Address::ZERO,
                to_address: addr,
                // Backfill keys on code_address (the implementation); in
                // production it equals the bytecode address (F14).
                code_address: Some(addr),
                codehash: None,
                call_type: "CALL".to_string(),
                selector: None,
                value_wei: None,
                gas_provided: 0,
                gas_used: 0,
                gas_margin: None,
                success: false,
                parent_gas_at_call: None,
                gas_requested_on_stack: None,
                eip150_cap_binding: None,
                deployed_bytecode_len: None,
                repricing_gas_delta: 0,
                ..Default::default()
            }],
            opcode_counts: vec![],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        };
        let coverage = BlockCoverageRow {
            schedule_name: "test".to_string(),
            schedule_config_hash: "cfg".to_string(),
            block_number: 1,
            block_hash: B256::repeat_byte(0x01),
            parent_hash: B256::ZERO,
            timestamp: 0,
            tx_count: 1,
            tx_count_unchanged: 0,
            tx_count_gas_only: 0,
            tx_count_stored: 1,
            block_gas_used: 15_000_000,
            block_gas_limit: 30_000_000,
            block_base_fee_per_gas: Some(1_000_000_000),
        };
        // block_hash needs to differ per call to satisfy the PK if we
        // seed multiple times.
        let mut coverage = coverage;
        coverage.block_hash = B256::repeat_byte(0x01 + tx_index as u8);
        coverage.block_number = 1 + tx_index as u64;
        let mut div = drill_in;
        div.divergence.block_number = coverage.block_number;
        let output = BlockOutput {
            coverage,
            summaries: vec![],
            drill_ins: vec![div],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        db.record_block_output(&output).unwrap();
    }

    #[test]
    fn run_metadata_backfill_upserts_per_distinct_codehash() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let fetcher = MockFetcher::new();

        // Two distinct addresses pointing at the SAME bytecode → one
        // codehash row.
        let addr_a = Address::repeat_byte(0xaa);
        let addr_b = Address::repeat_byte(0xbb);
        let shared_bytecode = {
            let mut bc = vec![0x60, 0x80, 0x60, 0x40, 0x52];
            bc.extend_from_slice(&solc_0_8_21_trailer());
            bc
        };
        fetcher.set(addr_a, Some(shared_bytecode.clone()));
        fetcher.set(addr_b, Some(shared_bytecode.clone()));
        seed_drill_in(&db, 0, addr_a);
        seed_drill_in(&db, 1, addr_b);

        let stats = run_metadata_backfill(&db, &fetcher).unwrap();
        assert_eq!(stats.addresses_examined, 2);
        assert_eq!(stats.upserted + stats.skipped_existing, 2);
        // Exactly one new row (the second address dedupes by codehash).
        assert_eq!(stats.upserted, 1);
        assert_eq!(stats.skipped_existing, 1);

        // Re-running is idempotent: both rows now exist → both skipped.
        let stats = run_metadata_backfill(&db, &fetcher).unwrap();
        assert_eq!(stats.upserted, 0);
        assert_eq!(stats.skipped_existing, 2);
    }

    #[test]
    fn run_metadata_backfill_counts_missing_and_failing_fetches() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let fetcher = MockFetcher::new();

        let missing = Address::repeat_byte(0x11);
        let failing = Address::repeat_byte(0x22);
        fetcher.set(missing, None);
        fetcher.set_failure(failing, "state read failed");

        seed_drill_in(&db, 0, missing);
        seed_drill_in(&db, 1, failing);

        let stats = run_metadata_backfill(&db, &fetcher).unwrap();
        assert_eq!(stats.addresses_examined, 2);
        assert_eq!(stats.no_bytecode, 1);
        assert_eq!(stats.fetch_errors, 1);
        assert_eq!(stats.upserted, 0);
    }

    /// Skip-value covers tagged values too — solc's `solc` field has been
    /// encoded as `tag(54, byteString)` in some compilations. Make sure the
    /// parser doesn't bail on unfamiliar keys carrying tagged payloads.
    #[test]
    fn skip_value_handles_tagged_payloads() {
        // map of 2: "extra": tag(54, [3,4,5]), "solc": [0,8,1]
        let mut cbor = Vec::new();
        cbor.push(0xa2);
        cbor.push(0x65); // text string "extra"
        cbor.extend_from_slice(b"extra");
        cbor.push(0xd8); // tag (1-byte arg)
        cbor.push(54);
        cbor.push(0x43); // byte string of 3
        cbor.push(0x03);
        cbor.push(0x04);
        cbor.push(0x05);
        cbor.push(0x64);
        cbor.extend_from_slice(b"solc");
        cbor.push(0x43);
        cbor.push(0x00);
        cbor.push(0x08);
        cbor.push(0x01);

        let len = cbor.len() as u16;
        let mut bytecode = Vec::new();
        bytecode.extend_from_slice(&cbor);
        bytecode.extend_from_slice(&len.to_be_bytes());

        let meta = parse_metadata_trailer(&bytecode).unwrap();
        assert!(meta.cbor_present);
        assert_eq!(meta.solc_version.as_deref(), Some("0.8.1"));
    }
}
