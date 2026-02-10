# Multi-Schedule Gas Research Architecture

## Overview

This document proposes an architecture for running multiple gas schedule experiments simultaneously, comparing each against the baseline Ethereum execution, and storing divergences in a unified database.

## Current State Analysis

### Main Branch
- Uses `gas_multiplier` approach - uniformly multiplies all gas costs
- Single schedule comparison (baseline vs multiplied)
- `GasResearchInspector` applies multiplier during execution

### eip-2780 Branch
- Classifies transactions into categories (NOP, TransferToEOA, CallToContract, etc.)
- Each category has different intrinsic gas costs
- Separate `eip2780_results` table in database
- Compares intrinsic gas only (not opcode-level)

### 7904_prelim_numbers Branch
- CSV-based per-opcode and per-precompile pricing
- `GasPricingTable` maps opcodes/precompiles to new costs
- Inspector charges additional gas based on pricing delta
- More granular than multiplier approach

## Proposed Architecture

### 1. Gas Schedule Trait

```rust
/// Defines how gas costs are modified for a specific experiment.
pub trait GasSchedule: Send + Sync + Clone + Debug {
    /// Unique identifier for this schedule (e.g., "eip-2780", "7904-v1")
    fn name(&self) -> &str;

    /// Human-readable description
    fn description(&self) -> &str;

    /// Calculate intrinsic gas for a transaction.
    /// Returns None to use default calculation.
    fn intrinsic_gas(&self, tx_context: &TxContext) -> Option<u64>;

    /// Get additional gas to charge for an opcode.
    /// Returns 0 if unaffected by this schedule.
    fn opcode_gas_delta(&self, opcode: u8, context: &OpcodeContext) -> u64;

    /// Get additional gas to charge for a precompile call.
    /// Returns 0 if unaffected by this schedule.
    fn precompile_gas_delta(&self, address: &Address, input: &[u8]) -> u64;

    /// Whether this schedule modifies intrinsic gas
    fn modifies_intrinsic(&self) -> bool;

    /// Whether this schedule modifies opcode/precompile gas
    fn modifies_execution(&self) -> bool;
}
```

### 2. Built-in Schedule Implementations

```rust
/// Baseline - no modifications (for comparison reference)
pub struct BaselineSchedule;

/// EIP-2780: Reduced intrinsic gas based on tx category
pub struct Eip2780Schedule {
    categories_filter: Option<Vec<Eip2780TxCategory>>,
}

/// CSV-based per-opcode/precompile pricing (EIP-7904 style)
pub struct CsvPricingSchedule {
    name: String,
    pricing_table: GasPricingTable,
}

/// Uniform multiplier (existing approach)
pub struct MultiplierSchedule {
    name: String,
    multiplier: u64,
}

/// Composite schedule combining multiple modifications
pub struct CompositeSchedule {
    name: String,
    intrinsic: Option<Box<dyn GasSchedule>>,
    execution: Option<Box<dyn GasSchedule>>,
}
```

### 3. Schedule Registry & Loading

```rust
/// Registry of available gas schedules
pub struct ScheduleRegistry {
    schedules: HashMap<String, Arc<dyn GasSchedule>>,
}

impl ScheduleRegistry {
    /// Create registry and load schedules from CLI args
    pub fn from_args(args: &ResearchArgs) -> Result<Self, ScheduleError> {
        let mut registry = Self::new();

        // Load EIP-2780 if enabled
        if args.eip2780_enabled {
            registry.register(Eip2780Schedule::new());
        }

        // Load all CSV schedules
        for csv_schedule in &args.csv_schedules {
            let pricing_table = GasPricingTable::from_csv_path(&csv_schedule.path)?;
            registry.register(CsvPricingSchedule::new(
                csv_schedule.name.clone(),
                pricing_table,
            ));
        }

        // Load all multiplier schedules
        for mult_schedule in &args.multiplier_schedules {
            registry.register(MultiplierSchedule::new(
                mult_schedule.name.clone(),
                mult_schedule.multiplier,
            ));
        }

        Ok(registry)
    }

    /// Register a new schedule
    pub fn register(&mut self, schedule: impl GasSchedule + 'static);

    /// Get all registered schedules
    pub fn all(&self) -> Vec<Arc<dyn GasSchedule>>;

    /// Get schedule by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn GasSchedule>>;

    /// List all schedule names
    pub fn names(&self) -> Vec<&str>;

    /// Summary for logging
    pub fn summary(&self) -> String {
        format!(
            "{} schedules: {}",
            self.schedules.len(),
            self.names().join(", ")
        )
    }
}

/// Errors during schedule loading
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("Failed to load CSV schedule '{name}': {source}")]
    CsvLoad { name: String, source: GasPricingError },

    #[error("Duplicate schedule name: {0}")]
    DuplicateName(String),

    #[error("No schedules configured")]
    NoSchedules,
}
```

**Startup Flow:**

```
CLI Args Parsed
     │
     ▼
ScheduleRegistry::from_args()
     │
     ├─> Validate all CSV files exist and parse correctly
     ├─> Check for duplicate names
     ├─> Log summary: "Loaded 4 schedules: eip-2780, 7904-v1, 7904-v2, 128x"
     │
     ▼
MultiScheduleInspector created with registry
     │
     ▼
ExEx starts processing blocks
```

### 4. Multi-Schedule Inspector

```rust
/// Inspector that tracks gas usage across multiple schedules simultaneously
pub struct MultiScheduleInspector {
    /// The schedules being tested
    schedules: Vec<Arc<dyn GasSchedule>>,

    /// Per-schedule tracking state
    schedule_states: Vec<ScheduleState>,

    /// Shared operation tracking (counts, call frames, etc.)
    shared_state: SharedInspectorState,
}

struct ScheduleState {
    schedule_name: String,
    additional_gas_charged: u64,
    oog_occurred: bool,
    oog_info: Option<OutOfGasInfo>,
    divergence_location: Option<DivergenceLocation>,
}
```

### 5. Multi-Schedule Comparison Result

```rust
/// Result of comparing a transaction across multiple schedules
pub struct MultiScheduleComparisonResult {
    /// Block and transaction info
    pub block_number: u64,
    pub tx_index: u64,
    pub tx_hash: B256,

    /// Baseline execution result
    pub baseline: ExecutionSummary,

    /// Per-schedule comparison results
    pub schedule_results: Vec<ScheduleComparisonResult>,
}

pub struct ScheduleComparisonResult {
    pub schedule_name: String,

    /// Intrinsic gas comparison (if schedule modifies intrinsic)
    pub intrinsic_comparison: Option<IntrinsicComparison>,

    /// Execution comparison (if schedule modifies opcodes/precompiles)
    pub execution_comparison: Option<ExecutionComparison>,

    /// Whether divergence occurred
    pub has_divergence: bool,

    /// Type of divergence
    pub divergence_type: Option<DivergenceType>,
}

pub struct IntrinsicComparison {
    pub baseline_intrinsic: u64,
    pub schedule_intrinsic: u64,
    pub delta: i64,
    pub tx_category: Option<String>, // For EIP-2780 style
}

pub struct ExecutionComparison {
    pub additional_gas_charged: u64,
    pub baseline_success: bool,
    pub schedule_success: bool,
    pub gas_efficiency_ratio: f64,
}
```

### 6. Unified Database Schema

```sql
-- Main divergences table (extended)
CREATE TABLE divergences (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    schedule_name TEXT NOT NULL,           -- Which schedule caused this
    block_number INTEGER NOT NULL,
    tx_index INTEGER NOT NULL,
    tx_hash BLOB NOT NULL,
    timestamp INTEGER NOT NULL,

    -- Divergence classification
    divergence_type TEXT NOT NULL,         -- status, state_root, gas_pattern, etc.

    -- Baseline execution
    baseline_success BOOLEAN NOT NULL,
    baseline_gas_used INTEGER NOT NULL,
    baseline_intrinsic_gas INTEGER NOT NULL,

    -- Schedule execution
    schedule_success BOOLEAN NOT NULL,
    schedule_gas_used INTEGER NOT NULL,    -- Additional gas for opcode schedules
    schedule_intrinsic_gas INTEGER,        -- For intrinsic-modifying schedules

    -- Analysis
    gas_delta INTEGER NOT NULL,
    gas_efficiency_ratio REAL,

    -- Context
    tx_category TEXT,                      -- For EIP-2780: nop_to_eoa, transfer_to_eoa, etc.
    affected_opcodes TEXT,                 -- JSON array of opcodes with changed costs
    affected_precompiles TEXT,             -- JSON array of precompiles with changed costs

    -- Detailed info (optional based on trace_detail)
    oog_info TEXT,                         -- JSON: opcode, pc, contract, pattern
    divergence_location TEXT,              -- JSON: first divergence point
    operation_counts TEXT,                 -- JSON: sload, sstore, etc.

    created_at INTEGER DEFAULT (strftime('%s', 'now'))
);

-- Indexes for common queries
CREATE INDEX idx_divergences_schedule ON divergences(schedule_name);
CREATE INDEX idx_divergences_block ON divergences(block_number);
CREATE INDEX idx_divergences_type ON divergences(divergence_type);
CREATE INDEX idx_divergences_category ON divergences(tx_category);

-- Summary statistics table (materialized for performance)
CREATE TABLE schedule_stats (
    schedule_name TEXT PRIMARY KEY,
    total_transactions INTEGER NOT NULL DEFAULT 0,
    total_divergences INTEGER NOT NULL DEFAULT 0,
    status_divergences INTEGER NOT NULL DEFAULT 0,
    total_gas_saved INTEGER NOT NULL DEFAULT 0,
    total_gas_increase INTEGER NOT NULL DEFAULT 0,
    last_block_processed INTEGER,
    updated_at INTEGER
);
```

### 7. CLI Configuration

```rust
/// Research mode CLI arguments
#[derive(Debug, Args)]
pub struct ResearchArgs {
    /// Enable research mode
    #[arg(long, default_value = "false")]
    pub research_enabled: bool,

    /// Enable EIP-2780 intrinsic gas experiment
    #[arg(long = "research.eip2780")]
    pub eip2780_enabled: bool,

    /// CSV pricing schedules: name=path pairs (can be specified multiple times)
    /// Example: --research.csv 7904-v1=./7904_prelim.csv --research.csv 7904-aggressive=./aggressive.csv
    #[arg(long = "research.csv", value_parser = parse_named_csv)]
    pub csv_schedules: Vec<NamedCsvSchedule>,

    /// Multiplier schedules: name=multiplier pairs (can be specified multiple times)
    /// Example: --research.multiplier 128x=128 --research.multiplier 256x=256
    #[arg(long = "research.multiplier", value_parser = parse_named_multiplier)]
    pub multiplier_schedules: Vec<NamedMultiplierSchedule>,

    /// Path to divergence database
    #[arg(long = "research.db-path", default_value = "research.db")]
    pub db_path: PathBuf,

    /// Block number to start analysis
    #[arg(long = "research.start-block", default_value = "0")]
    pub start_block: u64,

    /// Maximum parallel transactions per block
    #[arg(long = "research.parallel-txs")]
    pub parallel_txs: Option<usize>,

    /// Trace detail level (minimal, standard, detailed)
    #[arg(long = "research.trace-detail", default_value = "standard")]
    pub trace_detail: TraceDetail,
}

/// A named CSV schedule parsed from CLI
#[derive(Debug, Clone)]
pub struct NamedCsvSchedule {
    pub name: String,
    pub path: PathBuf,
}

/// A named multiplier schedule parsed from CLI
#[derive(Debug, Clone)]
pub struct NamedMultiplierSchedule {
    pub name: String,
    pub multiplier: u64,
}

fn parse_named_csv(s: &str) -> Result<NamedCsvSchedule, String> {
    let (name, path) = s.split_once('=')
        .ok_or_else(|| format!("Expected name=path format, got: {}", s))?;
    Ok(NamedCsvSchedule {
        name: name.to_string(),
        path: PathBuf::from(path),
    })
}

fn parse_named_multiplier(s: &str) -> Result<NamedMultiplierSchedule, String> {
    let (name, mult) = s.split_once('=')
        .ok_or_else(|| format!("Expected name=multiplier format, got: {}", s))?;
    let multiplier = mult.parse()
        .map_err(|_| format!("Invalid multiplier: {}", mult))?;
    Ok(NamedMultiplierSchedule { name: name.to_string(), multiplier })
}
```

### 8. Execution Flow

```
Block Committed (via ExEx notification)
           │
           ▼
┌─────────────────────────────────────────────────────────────┐
│  For each transaction in block:                              │
│                                                              │
│  1. Execute baseline (normal gas schedule)                   │
│     └─> Record: success, gas_used, receipts                  │
│                                                              │
│  2. For each enabled schedule (parallel):                    │
│     ├─> Calculate intrinsic gas (if schedule modifies it)    │
│     ├─> Re-execute with MultiScheduleInspector               │
│     │   └─> Inspector charges additional gas per schedule    │
│     ├─> Compare against baseline                             │
│     └─> Record divergences if any                            │
│                                                              │
│  3. Batch-write divergences to database                      │
└─────────────────────────────────────────────────────────────┘
           │
           ▼
    Send FinishedHeight event
```

### 9. File Structure

```
crates/research/
├── src/
│   ├── lib.rs                      # Public exports
│   ├── config.rs                   # ResearchConfig, TraceDetail
│   │
│   ├── schedule/                   # Gas schedule abstraction
│   │   ├── mod.rs                  # GasSchedule trait
│   │   ├── baseline.rs             # BaselineSchedule
│   │   ├── eip2780.rs              # Eip2780Schedule
│   │   ├── csv_pricing.rs          # CsvPricingSchedule (EIP-7904)
│   │   ├── multiplier.rs           # MultiplierSchedule
│   │   └── registry.rs             # ScheduleRegistry
│   │
│   ├── inspector/                  # Execution inspection
│   │   ├── mod.rs
│   │   ├── multi_schedule.rs       # MultiScheduleInspector
│   │   └── tracking.rs             # TrackingInspector (shared state)
│   │
│   ├── comparison/                 # Result comparison
│   │   ├── mod.rs
│   │   ├── result.rs               # MultiScheduleComparisonResult
│   │   └── analyzer.rs             # Comparison logic
│   │
│   ├── database/                   # Persistence
│   │   ├── mod.rs
│   │   ├── schema.rs               # Table definitions
│   │   ├── writer.rs               # Async batch writer
│   │   └── queries.rs              # Query helpers
│   │
│   ├── divergence.rs               # Divergence types
│   ├── metrics.rs                  # Prometheus metrics
│   └── executor.rs                 # ResearchExecutor wrapper
│
├── tests/
│   ├── schedule_tests.rs
│   ├── inspector_tests.rs
│   └── integration_tests.rs
│
└── Cargo.toml

bin/reth-research/
└── src/
    └── main.rs                     # ExEx implementation
```

### 10. Usage Examples

**Run with EIP-2780 only:**
```bash
reth-research node --dev \
  --research.eip2780 \
  --research.db-path ./eip2780-results.db
```

**Run with a single CSV pricing schedule:**
```bash
reth-research node --dev \
  --research.csv 7904-prelim=./7904_prelim_numbers.csv \
  --research.db-path ./results.db
```

**Run multiple CSV variations simultaneously:**
```bash
reth-research node --dev \
  --research.csv 7904-v1=./schedules/7904_v1.csv \
  --research.csv 7904-v2=./schedules/7904_v2.csv \
  --research.csv 7904-aggressive=./schedules/7904_aggressive.csv \
  --research.csv my-experiment=./schedules/custom.csv \
  --research.db-path ./multi-results.db
```

**Run EIP-2780 + multiple CSV schedules:**
```bash
reth-research node --dev \
  --research.eip2780 \
  --research.csv 7904-v1=./7904_v1.csv \
  --research.csv 7904-conservative=./7904_conservative.csv \
  --research.db-path ./combined-results.db
```

**Run with uniform multipliers for comparison:**
```bash
reth-research node --dev \
  --research.multiplier 128x=128 \
  --research.multiplier 256x=256 \
  --research.db-path ./multiplier-results.db
```

**Kitchen sink - all schedule types:**
```bash
reth-research node --dev \
  --research.eip2780 \
  --research.csv 7904-prelim=./7904_prelim.csv \
  --research.csv 7904-aggressive=./7904_aggressive.csv \
  --research.multiplier 128x=128 \
  --research.db-path ./full-analysis.db \
  --research.start-block 19000000 \
  --research.trace-detail detailed
```

**Query results:**
```sql
-- Divergences by schedule
SELECT schedule_name, COUNT(*) as divergences
FROM divergences
GROUP BY schedule_name
ORDER BY divergences DESC;

-- Compare two CSV schedules side-by-side
SELECT
    d1.tx_hash,
    d1.schedule_name as schedule_1,
    d1.gas_delta as delta_1,
    d2.schedule_name as schedule_2,
    d2.gas_delta as delta_2
FROM divergences d1
JOIN divergences d2 ON d1.tx_hash = d2.tx_hash
WHERE d1.schedule_name = '7904-v1'
  AND d2.schedule_name = '7904-aggressive';

-- EIP-2780 category breakdown
SELECT tx_category,
       COUNT(*) as count,
       SUM(gas_delta) as total_gas_delta,
       AVG(gas_delta) as avg_gas_delta
FROM divergences
WHERE schedule_name = 'eip-2780'
GROUP BY tx_category
ORDER BY total_gas_delta DESC;

-- Which opcodes cause most divergences across all CSV schedules?
SELECT
    schedule_name,
    json_each.value as opcode,
    COUNT(*) as divergence_count
FROM divergences, json_each(affected_opcodes)
WHERE schedule_name LIKE '7904%'
GROUP BY schedule_name, json_each.value
ORDER BY divergence_count DESC;

-- Find transactions that diverge under some schedules but not others
SELECT tx_hash, GROUP_CONCAT(schedule_name) as divergent_schedules
FROM divergences
GROUP BY tx_hash
HAVING COUNT(*) < (SELECT COUNT(DISTINCT schedule_name) FROM divergences);
```

### 11. CSV Schedule Format

The CSV format for custom pricing schedules follows the pattern from 7904:

```csv
Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
SDIV,constant,5,15
MOD,constant,5,15
ADDMOD,constant,8,8
MULMOD,constant,8,24
KECCAK256,constant,30,45
KECCAK256,msg_size,6,6
ECPAIRING,constant,45000,45000
ECPAIRING,num_pairs,34000,34103
BLAKE2F,constant,0,170
BLAKE2F,num_rounds,1,2
```

**Columns:**
- `Opcode`: Name of the opcode or precompile (DIV, KECCAK256, ECPAIRING, etc.)
- `Parameter`: Type of cost (`constant` for base cost, or variable type like `msg_size`, `num_pairs`, `num_rounds`)
- `Current Gas`: The current mainnet gas cost
- `New Gas`: The proposed new gas cost

**Supported Opcodes:**
- Arithmetic: `DIV`, `SDIV`, `MOD`, `SMOD`, `ADDMOD`, `MULMOD`, `EXP`
- Crypto: `KECCAK256` (alias: `SHA3`)
- Memory: `MLOAD`, `MSTORE`, `MSTORE8`
- (extensible - add more as needed)

**Supported Precompiles:**
- `ECRECOVER` (0x01), `SHA256` (0x02), `RIPEMD160` (0x03)
- `IDENTITY` (0x04), `MODEXP` (0x05)
- `ECADD` (0x06), `ECMUL` (0x07), `ECPAIRING` (0x08)
- `BLAKE2F` (0x09), `POINT_EVALUATION` (0x0a)
- BLS12-381: `BLS12_G1ADD`, `BLS12_G2ADD`, etc.

**Variable Parameters:**
- `msg_size`: For `KECCAK256` - number of 32-byte words
- `num_pairs`: For `ECPAIRING` - number of point pairs (input.len() / 192)
- `num_rounds`: For `BLAKE2F` - from first 4 bytes of input

### 12. Schedule Composition

When multiple schedules are enabled, they operate independently:

```
Transaction arrives
     │
     ├─> Baseline: Execute normally
     │
     ├─> EIP-2780: Calculate intrinsic gas delta only
     │   (no re-execution needed - pure calculation)
     │
     ├─> 7904-v1: Execute with CSV pricing inspector
     │   (charges additional gas based on opcode delta)
     │
     ├─> 7904-aggressive: Execute with different CSV pricing
     │   (same mechanism, different numbers)
     │
     └─> Each produces independent comparison result
```

For **combined experiments** (e.g., "what if we did EIP-2780 AND EIP-7904 together?"), use a composite schedule:

```bash
# Future enhancement: composite schedules
--research.composite "eip2780+7904=eip2780,7904-v1"
```

This would apply both intrinsic gas changes AND opcode repricing in a single execution.

## Implementation Strategy

### Phase 1: Core Abstraction (Foundation)
1. Create `GasSchedule` trait
2. Implement `BaselineSchedule`
3. Create `ScheduleRegistry`
4. Update database schema with `schedule_name`

### Phase 2: Migrate Existing Schedules
1. Port `Eip2780Schedule` from eip-2780 branch
2. Port `CsvPricingSchedule` from 7904_prelim_numbers branch
3. Keep `MultiplierSchedule` for backwards compatibility

### Phase 3: Multi-Schedule Inspector
1. Create `MultiScheduleInspector`
2. Track per-schedule state during single execution pass
3. Compare and record divergences

### Phase 4: Integration & CLI
1. Update CLI with new flags
2. Create ExEx that uses multi-schedule system
3. Add parallel processing support

### Phase 5: Testing & Documentation
1. Unit tests for each schedule type
2. Integration tests with test blocks
3. Documentation and examples

## Design Principles

1. **Single Execution Pass**: Re-executing the entire block for each schedule is expensive. Instead, the inspector tracks state for all schedules during a single execution and determines which would have diverged.

2. **Lazy Comparison**: For schedules that only modify intrinsic gas (like EIP-2780), we don't need to re-execute at all - just calculate what the intrinsic gas would have been.

3. **Unified Storage**: All divergences go to the same database with `schedule_name` differentiation, enabling cross-schedule analysis.

4. **Extensibility**: Adding a new gas schedule only requires implementing the `GasSchedule` trait - no changes to core infrastructure.

5. **Backwards Compatibility**: Existing `gas_multiplier` approach maps to `MultiplierSchedule`.

## Experimentation Workflow

The architecture is designed to make experimentation fast and easy:

### Adding a New Gas Schedule Experiment

**Option 1: New CSV file (no code changes)**
```bash
# Create your CSV with proposed gas costs
vim ./experiments/my_hypothesis.csv

# Run it alongside existing schedules
reth-research node --dev \
  --research.csv my-hypothesis=./experiments/my_hypothesis.csv \
  --research.csv 7904-baseline=./7904_prelim.csv \
  --research.db-path ./comparison.db
```

**Option 2: New schedule type (code change required)**
```rust
// Implement GasSchedule trait
pub struct MyCustomSchedule { /* ... */ }

impl GasSchedule for MyCustomSchedule {
    fn name(&self) -> &str { "my-custom" }
    // ...
}

// Register in CLI handling
if args.my_custom_enabled {
    registry.register(MyCustomSchedule::new(args.my_custom_params));
}
```

### Iterating on Numbers

```bash
# Run baseline
reth-research node --dev \
  --research.csv baseline=./schedules/7904_v1.csv \
  --research.db-path ./v1.db

# Analyze results
sqlite3 ./v1.db "SELECT COUNT(*) FROM divergences WHERE has_divergence=1"

# Adjust numbers in CSV, run again
vim ./schedules/7904_v2.csv

reth-research node --dev \
  --research.csv v1=./schedules/7904_v1.csv \
  --research.csv v2=./schedules/7904_v2.csv \
  --research.db-path ./comparison.db

# Compare v1 vs v2
sqlite3 ./comparison.db <<EOF
SELECT
    schedule_name,
    COUNT(*) as divergences,
    SUM(CASE WHEN divergence_type='status' THEN 1 ELSE 0 END) as status_failures,
    AVG(gas_delta) as avg_gas_delta
FROM divergences
GROUP BY schedule_name;
EOF
```

### A/B Testing Multiple Hypotheses

```bash
# Test 5 different approaches simultaneously
reth-research node --dev \
  --research.csv conservative=./schedules/conservative.csv \
  --research.csv moderate=./schedules/moderate.csv \
  --research.csv aggressive=./schedules/aggressive.csv \
  --research.csv alternative-a=./schedules/alt_a.csv \
  --research.csv alternative-b=./schedules/alt_b.csv \
  --research.db-path ./hypothesis-test.db

# Find which schedule has fewest divergences
sqlite3 ./hypothesis-test.db \
  "SELECT schedule_name, COUNT(*) FROM divergences GROUP BY 1 ORDER BY 2"
```

## Questions for Consideration

1. **Intrinsic-only schedules**: Should EIP-2780 style schedules that only change intrinsic gas even re-execute? We can determine divergence purely from the gas calculation.

2. **State isolation**: When re-executing under a different schedule, should we start from the same pre-tx state or simulate the cumulative effect of all prior txs under that schedule?

3. **Block-level vs tx-level**: Some schedules might only make sense at block level (e.g., different block gas limits). Should we support this?

4. **Memory constraints**: Running many schedules simultaneously might use significant memory. Should we add schedule batching for very large schedule sets?

5. **Historical analysis**: Should we support analyzing already-synced blocks from the database, or only live/streaming analysis?
