//! CSV-based per-opcode gas pricing schedule.
//!
//! This schedule allows loading custom gas costs from CSV files for
//! per-opcode and per-precompile repricing experiments.

use super::{
    context::OpcodeContext,
    traits::{GasSchedule, ScheduleKind},
};
use alloy_primitives::Address;
use std::{collections::HashMap, io::Read, path::Path};
use thiserror::Error;

/// Errors that can occur when loading gas pricing data.
#[derive(Debug, Error)]
pub enum CsvPricingError {
    /// IO error reading the CSV file
    #[error("Failed to read CSV file: {0}")]
    Io(#[from] std::io::Error),

    /// CSV parsing error
    #[error("Failed to parse CSV: {0}")]
    Csv(#[from] csv::Error),

    /// Invalid opcode name in CSV
    #[error("Unknown opcode in CSV: {0}")]
    UnknownOpcode(String),

    /// Invalid parameter type in CSV
    #[error("Unknown parameter type: {0}")]
    UnknownParameter(String),
}

/// Pricing for a single operation (opcode or precompile).
#[derive(Debug, Clone, Default)]
pub struct OperationPricing {
    /// Current constant gas cost
    pub current_constant: u64,
    /// New constant gas cost
    pub new_constant: u64,
    /// Current variable cost per unit
    pub current_variable: Option<u64>,
    /// New variable cost per unit
    pub new_variable: Option<u64>,
    /// Type of variable cost (e.g., "num_rounds", "msg_size", "num_pairs")
    pub variable_type: Option<String>,
}

impl OperationPricing {
    /// Calculate the gas delta (new - current) for a given number of variable units.
    pub fn gas_delta(&self, variable_units: u64) -> i64 {
        let current_total = self.current_constant +
            self.current_variable.map(|v| v.saturating_mul(variable_units)).unwrap_or(0);
        let new_total = self.new_constant +
            self.new_variable.map(|v| v.saturating_mul(variable_units)).unwrap_or(0);

        new_total as i64 - current_total as i64
    }

    /// Get the new total gas cost.
    pub fn new_total_gas(&self, variable_units: u64) -> u64 {
        self.new_constant + self.new_variable.map(|v| v.saturating_mul(variable_units)).unwrap_or(0)
    }
}

/// Gas pricing table loaded from CSV.
#[derive(Debug, Clone, Default)]
pub struct GasPricingTable {
    /// Opcode pricing by opcode byte
    pub opcodes: HashMap<u8, OperationPricing>,
    /// Precompile pricing by address
    pub precompiles: HashMap<Address, OperationPricing>,
}

impl GasPricingTable {
    /// Create an empty pricing table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load pricing from a CSV file path.
    pub fn from_csv_path(path: &Path) -> Result<Self, CsvPricingError> {
        let file = std::fs::File::open(path)?;
        Self::from_csv(file)
    }

    /// Load pricing from a CSV reader.
    pub fn from_csv<R: Read>(reader: R) -> Result<Self, CsvPricingError> {
        let mut csv_reader = csv::Reader::from_reader(reader);

        let mut opcode_entries: HashMap<String, OperationPricing> = HashMap::new();
        let mut precompile_entries: HashMap<String, OperationPricing> = HashMap::new();

        for result in csv_reader.records() {
            let record = result?;

            let opcode_name = record.get(0).unwrap_or("").trim();
            let parameter = record.get(1).unwrap_or("").trim();
            let current_gas: u64 = record.get(2).unwrap_or("0").trim().parse().unwrap_or(0);
            let new_gas: u64 = record.get(3).unwrap_or("0").trim().parse().unwrap_or(0);

            // Determine if this is an opcode or precompile
            let is_precompile = is_precompile_name(opcode_name);
            let entries = if is_precompile { &mut precompile_entries } else { &mut opcode_entries };

            let entry = entries.entry(opcode_name.to_string()).or_default();

            match parameter {
                "constant" => {
                    entry.current_constant = current_gas;
                    entry.new_constant = new_gas;
                }
                "num_rounds" | "msg_size" | "num_pairs" | "exp_bytes" | "words" => {
                    entry.current_variable = Some(current_gas);
                    entry.new_variable = Some(new_gas);
                    entry.variable_type = Some(parameter.to_string());
                }
                other if !other.is_empty() => {
                    return Err(CsvPricingError::UnknownParameter(other.to_string()));
                }
                _ => {}
            }
        }

        // Convert to final format
        let mut table = Self::new();

        for (name, pricing) in opcode_entries {
            if let Some(opcode_byte) = opcode_name_to_byte(&name) {
                table.opcodes.insert(opcode_byte, pricing);
            } else {
                return Err(CsvPricingError::UnknownOpcode(name));
            }
        }

        for (name, pricing) in precompile_entries {
            if let Some(address) = precompile_name_to_address(&name) {
                table.precompiles.insert(address, pricing);
            } else {
                return Err(CsvPricingError::UnknownOpcode(name));
            }
        }

        Ok(table)
    }

    /// Get pricing for an opcode.
    pub fn get_opcode(&self, opcode: u8) -> Option<&OperationPricing> {
        self.opcodes.get(&opcode)
    }

    /// Get pricing for a precompile.
    pub fn get_precompile(&self, address: &Address) -> Option<&OperationPricing> {
        self.precompiles.get(address)
    }

    /// Get the number of repriced opcodes.
    pub fn opcode_count(&self) -> usize {
        self.opcodes.len()
    }

    /// Get the number of repriced precompiles.
    pub fn precompile_count(&self) -> usize {
        self.precompiles.len()
    }

    /// Get all affected opcode bytes.
    pub fn affected_opcodes(&self) -> Vec<u8> {
        self.opcodes.keys().copied().collect()
    }

    /// Get all affected precompile addresses.
    pub fn affected_precompiles(&self) -> Vec<Address> {
        self.precompiles.keys().copied().collect()
    }
}

/// Check if a name refers to a precompile.
fn is_precompile_name(name: &str) -> bool {
    matches!(
        name,
        "ECRECOVER" |
            "SHA256" |
            "RIPEMD160" |
            "IDENTITY" |
            "MODEXP" |
            "ECADD" |
            "ECMUL" |
            "ECPAIRING" |
            "BLAKE2F" |
            "POINT_EVALUATION" |
            "BLS12_G1ADD" |
            "BLS12_G1MUL" |
            "BLS12_G1MSM" |
            "BLS12_G2ADD" |
            "BLS12_G2MUL" |
            "BLS12_G2MSM" |
            "BLS12_PAIRING" |
            "BLS12_MAP_FP_TO_G1" |
            "BLS12_MAP_FP2_TO_G2"
    )
}

/// Convert an opcode name to its byte value.
fn opcode_name_to_byte(name: &str) -> Option<u8> {
    match name {
        "STOP" => Some(0x00),
        "ADD" => Some(0x01),
        "MUL" => Some(0x02),
        "SUB" => Some(0x03),
        "DIV" => Some(0x04),
        "SDIV" => Some(0x05),
        "MOD" => Some(0x06),
        "SMOD" => Some(0x07),
        "ADDMOD" => Some(0x08),
        "MULMOD" => Some(0x09),
        "EXP" => Some(0x0A),
        "SIGNEXTEND" => Some(0x0B),
        "LT" => Some(0x10),
        "GT" => Some(0x11),
        "SLT" => Some(0x12),
        "SGT" => Some(0x13),
        "EQ" => Some(0x14),
        "ISZERO" => Some(0x15),
        "AND" => Some(0x16),
        "OR" => Some(0x17),
        "XOR" => Some(0x18),
        "NOT" => Some(0x19),
        "BYTE" => Some(0x1A),
        "SHL" => Some(0x1B),
        "SHR" => Some(0x1C),
        "SAR" => Some(0x1D),
        "KECCAK256" | "SHA3" => Some(0x20),
        "ADDRESS" => Some(0x30),
        "BALANCE" => Some(0x31),
        "ORIGIN" => Some(0x32),
        "CALLER" => Some(0x33),
        "CALLVALUE" => Some(0x34),
        "CALLDATALOAD" => Some(0x35),
        "CALLDATASIZE" => Some(0x36),
        "CALLDATACOPY" => Some(0x37),
        "CODESIZE" => Some(0x38),
        "CODECOPY" => Some(0x39),
        "GASPRICE" => Some(0x3A),
        "EXTCODESIZE" => Some(0x3B),
        "EXTCODECOPY" => Some(0x3C),
        "RETURNDATASIZE" => Some(0x3D),
        "RETURNDATACOPY" => Some(0x3E),
        "EXTCODEHASH" => Some(0x3F),
        "BLOCKHASH" => Some(0x40),
        "COINBASE" => Some(0x41),
        "TIMESTAMP" => Some(0x42),
        "NUMBER" => Some(0x43),
        "PREVRANDAO" | "DIFFICULTY" => Some(0x44),
        "GASLIMIT" => Some(0x45),
        "CHAINID" => Some(0x46),
        "SELFBALANCE" => Some(0x47),
        "BASEFEE" => Some(0x48),
        "BLOBHASH" => Some(0x49),
        "BLOBBASEFEE" => Some(0x4A),
        "POP" => Some(0x50),
        "MLOAD" => Some(0x51),
        "MSTORE" => Some(0x52),
        "MSTORE8" => Some(0x53),
        "SLOAD" => Some(0x54),
        "SSTORE" => Some(0x55),
        "JUMP" => Some(0x56),
        "JUMPI" => Some(0x57),
        "PC" => Some(0x58),
        "MSIZE" => Some(0x59),
        "GAS" => Some(0x5A),
        "JUMPDEST" => Some(0x5B),
        "TLOAD" => Some(0x5C),
        "TSTORE" => Some(0x5D),
        "MCOPY" => Some(0x5E),
        "PUSH0" => Some(0x5F),
        "CREATE" => Some(0xF0),
        "CALL" => Some(0xF1),
        "CALLCODE" => Some(0xF2),
        "RETURN" => Some(0xF3),
        "DELEGATECALL" => Some(0xF4),
        "CREATE2" => Some(0xF5),
        "STATICCALL" => Some(0xFA),
        "REVERT" => Some(0xFD),
        "INVALID" => Some(0xFE),
        "SELFDESTRUCT" => Some(0xFF),
        _ => None,
    }
}

/// Convert a precompile name to its address.
fn precompile_name_to_address(name: &str) -> Option<Address> {
    let addr_byte = match name {
        "ECRECOVER" => 0x01,
        "SHA256" => 0x02,
        "RIPEMD160" => 0x03,
        "IDENTITY" => 0x04,
        "MODEXP" => 0x05,
        "ECADD" => 0x06,
        "ECMUL" => 0x07,
        "ECPAIRING" => 0x08,
        "BLAKE2F" => 0x09,
        "POINT_EVALUATION" => 0x0A,
        "BLS12_G1ADD" => 0x0B,
        "BLS12_G1MUL" => 0x0C,
        "BLS12_G1MSM" => 0x0D,
        "BLS12_G2ADD" => 0x0E,
        "BLS12_G2MUL" => 0x0F,
        "BLS12_G2MSM" => 0x10,
        "BLS12_PAIRING" => 0x11,
        "BLS12_MAP_FP_TO_G1" => 0x12,
        "BLS12_MAP_FP2_TO_G2" => 0x13,
        _ => return None,
    };

    let mut addr_bytes = [0u8; 20];
    addr_bytes[19] = addr_byte;
    Some(Address::from(addr_bytes))
}

/// KECCAK256 opcode byte.
const OPCODE_KECCAK256: u8 = 0x20;

/// EXP opcode byte.
const OPCODE_EXP: u8 = 0x0A;

/// CSV-based gas pricing schedule.
#[derive(Debug, Clone)]
pub struct CsvPricingSchedule {
    /// Schedule name
    name: String,
    /// Pricing table loaded from CSV
    pricing_table: GasPricingTable,
}

impl CsvPricingSchedule {
    /// Create a new CSV pricing schedule from a pricing table.
    pub fn new(name: String, pricing_table: GasPricingTable) -> Self {
        Self { name, pricing_table }
    }

    /// Load a schedule from a CSV file path.
    pub fn from_path(name: String, path: &Path) -> Result<Self, CsvPricingError> {
        let pricing_table = GasPricingTable::from_csv_path(path)?;
        Ok(Self { name, pricing_table })
    }

    /// Load a schedule from CSV data.
    pub fn from_csv<R: Read>(name: String, reader: R) -> Result<Self, CsvPricingError> {
        let pricing_table = GasPricingTable::from_csv(reader)?;
        Ok(Self { name, pricing_table })
    }

    /// Get the pricing table.
    pub fn pricing_table(&self) -> &GasPricingTable {
        &self.pricing_table
    }

    /// Get variable units for KECCAK256.
    fn get_keccak_units(ctx: &OpcodeContext) -> u64 {
        ctx.keccak_words()
    }

    /// Get variable units for EXP (exponent byte size).
    fn get_exp_units(ctx: &OpcodeContext) -> u64 {
        ctx.exp_byte_size.unwrap_or(0) as u64
    }

    /// Get variable units for precompile calls.
    fn get_precompile_units(address: &Address, input: &[u8]) -> u64 {
        let addr_byte = address.0[19];

        match addr_byte {
            // ECPAIRING: num_pairs = input.len() / 192
            0x08 => (input.len() / 192) as u64,
            // BLAKE2F: num_rounds from first 4 bytes
            0x09 => {
                if input.len() >= 4 {
                    u32::from_be_bytes([input[0], input[1], input[2], input[3]]) as u64
                } else {
                    0
                }
            }
            // MODEXP and others: constant cost
            _ => 0,
        }
    }
}

impl GasSchedule for CsvPricingSchedule {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "CSV-based per-opcode/precompile gas pricing"
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::ExecutionOnly
    }

    fn opcode_gas_delta(&self, opcode: u8, ctx: &OpcodeContext) -> i64 {
        let Some(pricing) = self.pricing_table.get_opcode(opcode) else {
            return 0;
        };

        let variable_units = if opcode == OPCODE_KECCAK256 {
            Self::get_keccak_units(ctx)
        } else if opcode == OPCODE_EXP {
            Self::get_exp_units(ctx)
        } else {
            0
        };

        pricing.gas_delta(variable_units)
    }

    fn precompile_gas_delta(&self, address: &Address, input: &[u8]) -> i64 {
        let Some(pricing) = self.pricing_table.get_precompile(address) else {
            return 0;
        };

        let variable_units = Self::get_precompile_units(address, input);
        pricing.gas_delta(variable_units)
    }

    fn affected_opcodes(&self) -> Vec<u8> {
        self.pricing_table.affected_opcodes()
    }

    fn affected_precompiles(&self) -> Vec<Address> {
        self.pricing_table.affected_precompiles()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CSV: &str = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
SDIV,constant,5,15
KECCAK256,constant,30,45
KECCAK256,msg_size,6,6
ECPAIRING,constant,45000,45000
ECPAIRING,num_pairs,34000,34103
BLAKE2F,constant,0,170
BLAKE2F,num_rounds,1,2
"#;

    #[test]
    fn test_load_csv() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();

        assert_eq!(table.opcode_count(), 3); // DIV, SDIV, KECCAK256
        assert_eq!(table.precompile_count(), 2); // ECPAIRING, BLAKE2F
    }

    #[test]
    fn test_opcode_pricing() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();

        // DIV: 5 -> 15
        let div = table.get_opcode(0x04).unwrap();
        assert_eq!(div.current_constant, 5);
        assert_eq!(div.new_constant, 15);
        assert_eq!(div.gas_delta(0), 10); // 15 - 5

        // KECCAK256 with variable
        let keccak = table.get_opcode(OPCODE_KECCAK256).unwrap();
        assert_eq!(keccak.current_constant, 30);
        assert_eq!(keccak.new_constant, 45);
        // 10 words: current = 30 + 10*6 = 90, new = 45 + 10*6 = 105, delta = 15
        assert_eq!(keccak.gas_delta(10), 15);
    }

    #[test]
    fn test_precompile_pricing() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();

        let ecpairing_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]);
        let pricing = table.get_precompile(&ecpairing_addr).unwrap();

        // 3 pairs: current = 45000 + 3*34000 = 147000, new = 45000 + 3*34103 = 147309
        assert_eq!(pricing.gas_delta(3), 309);
    }

    #[test]
    fn test_schedule_opcode_delta() {
        let schedule =
            CsvPricingSchedule::from_csv("test".to_string(), TEST_CSV.as_bytes()).unwrap();

        let ctx = OpcodeContext::default();

        // DIV: +10 gas
        assert_eq!(schedule.opcode_gas_delta(0x04, &ctx), 10);

        // ADD: not in CSV, delta = 0
        assert_eq!(schedule.opcode_gas_delta(0x01, &ctx), 0);
    }

    #[test]
    fn test_schedule_precompile_delta() {
        let schedule =
            CsvPricingSchedule::from_csv("test".to_string(), TEST_CSV.as_bytes()).unwrap();

        let ecpairing_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]);

        // 3 pairs = 576 bytes
        let input = vec![0u8; 576];
        let delta = schedule.precompile_gas_delta(&ecpairing_addr, &input);
        assert_eq!(delta, 309);
    }

    #[test]
    fn test_affected_opcodes() {
        let schedule =
            CsvPricingSchedule::from_csv("test".to_string(), TEST_CSV.as_bytes()).unwrap();

        let affected = schedule.affected_opcodes();
        assert!(affected.contains(&0x04)); // DIV
        assert!(affected.contains(&0x05)); // SDIV
        assert!(affected.contains(&0x20)); // KECCAK256
    }
}
