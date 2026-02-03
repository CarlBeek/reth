//! Gas pricing tables for EIP-7904 research.
//!
//! This module provides per-opcode and per-precompile gas pricing loaded from CSV files.
//! The pricing data is used to simulate proposed gas cost changes during dual execution.

use alloy_primitives::Address;
use std::{collections::HashMap, io::Read, path::Path};
use thiserror::Error;

/// Errors that can occur when loading gas pricing data.
#[derive(Debug, Error)]
pub enum GasPricingError {
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

    /// Missing required constant cost
    #[error("Missing constant cost for operation: {0}")]
    MissingConstant(String),
}

/// Pricing for a single operation (opcode or precompile).
#[derive(Debug, Clone, Default)]
pub struct OperationPricing {
    /// Current constant gas cost
    pub current_constant: u64,
    /// New constant gas cost
    pub new_constant: u64,
    /// Current variable cost per unit (for operations with variable costs)
    pub current_variable: Option<u64>,
    /// New variable cost per unit
    pub new_variable: Option<u64>,
    /// Type of variable cost (e.g., "num_rounds", "msg_size", "num_pairs")
    pub variable_type: Option<String>,
}

impl OperationPricing {
    /// Get the new constant gas cost to charge.
    pub fn new_constant_gas(&self) -> u64 {
        self.new_constant
    }

    /// Calculate the new variable gas cost.
    /// Returns 0 if no variable cost defined.
    pub fn new_variable_gas(&self, units: u64) -> u64 {
        self.new_variable.map(|v| units.saturating_mul(v)).unwrap_or(0)
    }

    /// Calculate total new gas including both constant and variable components.
    pub fn total_new_gas(&self, variable_units: u64) -> u64 {
        self.new_constant_gas().saturating_add(self.new_variable_gas(variable_units))
    }
}

/// Gas pricing table loaded from CSV.
///
/// Maps opcode bytes and precompile addresses to their pricing information.
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
    pub fn from_csv_path(path: &Path) -> Result<Self, GasPricingError> {
        let file = std::fs::File::open(path)?;
        Self::from_csv(file)
    }

    /// Load pricing from a CSV reader.
    pub fn from_csv<R: Read>(reader: R) -> Result<Self, GasPricingError> {
        let mut csv_reader = csv::Reader::from_reader(reader);

        // Temporary storage for building pricing entries
        let mut opcode_entries: HashMap<String, OperationPricing> = HashMap::new();
        let mut precompile_entries: HashMap<String, OperationPricing> = HashMap::new();

        for result in csv_reader.records() {
            let record = result?;

            let opcode_name = record.get(0).unwrap_or("").trim();
            let parameter = record.get(1).unwrap_or("").trim();
            let current_gas: u64 = record.get(2).unwrap_or("0").trim().parse().unwrap_or(0);
            let new_gas: u64 = record.get(3).unwrap_or("0").trim().parse().unwrap_or(0);

            // Determine if this is an opcode or precompile
            let is_precompile = matches!(
                opcode_name,
                "BLAKE2F" |
                    "BLS12_G1ADD" |
                    "BLS12_G2ADD" |
                    "ECADD" |
                    "ECPAIRING" |
                    "ECRECOVER" |
                    "POINT_EVALUATION"
            );

            let entries = if is_precompile { &mut precompile_entries } else { &mut opcode_entries };

            let entry = entries.entry(opcode_name.to_string()).or_default();

            match parameter {
                "constant" => {
                    entry.current_constant = current_gas;
                    entry.new_constant = new_gas;
                }
                "num_rounds" | "msg_size" | "num_pairs" => {
                    entry.current_variable = Some(current_gas);
                    entry.new_variable = Some(new_gas);
                    entry.variable_type = Some(parameter.to_string());
                }
                _ => {
                    return Err(GasPricingError::UnknownParameter(parameter.to_string()));
                }
            }
        }

        // Convert to final format with opcode bytes and addresses
        let mut table = Self::new();

        for (name, pricing) in opcode_entries {
            if let Some(opcode_byte) = opcode_name_to_byte(&name) {
                table.opcodes.insert(opcode_byte, pricing);
            } else {
                return Err(GasPricingError::UnknownOpcode(name));
            }
        }

        for (name, pricing) in precompile_entries {
            if let Some(address) = precompile_name_to_address(&name) {
                table.precompiles.insert(address, pricing);
            } else {
                return Err(GasPricingError::UnknownOpcode(name));
            }
        }

        Ok(table)
    }

    /// Get pricing for an opcode, if present in the table.
    pub fn get_opcode_pricing(&self, opcode: u8) -> Option<&OperationPricing> {
        self.opcodes.get(&opcode)
    }

    /// Get pricing for a precompile address, if present in the table.
    pub fn get_precompile_pricing(&self, address: &Address) -> Option<&OperationPricing> {
        self.precompiles.get(address)
    }

    /// Check if an address is a repriced precompile.
    pub fn is_repriced_precompile(&self, address: &Address) -> bool {
        self.precompiles.contains_key(address)
    }

    /// Get the number of repriced opcodes.
    pub fn opcode_count(&self) -> usize {
        self.opcodes.len()
    }

    /// Get the number of repriced precompiles.
    pub fn precompile_count(&self) -> usize {
        self.precompiles.len()
    }
}

/// Convert an opcode name to its byte value.
fn opcode_name_to_byte(name: &str) -> Option<u8> {
    match name {
        "DIV" => Some(0x04),
        "SDIV" => Some(0x05),
        "MOD" => Some(0x06),
        "SMOD" => Some(0x07),
        "ADDMOD" => Some(0x08),
        "MULMOD" => Some(0x09),
        "KECCAK256" | "SHA3" => Some(0x20),
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
        "POINT_EVALUATION" => 0x0a,
        "BLS12_G1ADD" => 0x0b,
        "BLS12_G1MUL" => 0x0c,
        "BLS12_G1MSM" => 0x0d,
        "BLS12_G2ADD" => 0x0e,
        "BLS12_G2MUL" => 0x0f,
        "BLS12_G2MSM" => 0x10,
        "BLS12_PAIRING" => 0x11,
        "BLS12_MAP_FP_TO_G1" => 0x12,
        "BLS12_MAP_FP2_TO_G2" => 0x13,
        _ => return None,
    };

    // Precompile addresses are 0x0000...0001 through 0x0000...0013
    let mut addr_bytes = [0u8; 20];
    addr_bytes[19] = addr_byte;
    Some(Address::from(addr_bytes))
}

/// KECCAK256 opcode byte
pub const OPCODE_KECCAK256: u8 = 0x20;

/// Precompile addresses for reference
pub mod precompile_addresses {
    use alloy_primitives::Address;

    /// ECRECOVER precompile (0x01)
    pub const ECRECOVER: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01]);

    /// ECADD precompile (0x06)
    pub const ECADD: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x06]);

    /// ECPAIRING precompile (0x08)
    pub const ECPAIRING: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]);

    /// BLAKE2F precompile (0x09)
    pub const BLAKE2F: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x09]);

    /// POINT_EVALUATION precompile (0x0a)
    pub const POINT_EVALUATION: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0a]);

    /// BLS12_G1ADD precompile (0x0b)
    pub const BLS12_G1ADD: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0b]);

    /// BLS12_G2ADD precompile (0x0e)
    pub const BLS12_G2ADD: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0e]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CSV: &str = r#"Opcode,Parameter,Current Gas,New Gas
ADDMOD,constant,8,8
DIV,constant,5,15
KECCAK256,constant,30,45
KECCAK256,msg_size,6,6
ECPAIRING,constant,45000,45000
ECPAIRING,num_pairs,34000,34103
BLAKE2F,constant,0,170
BLAKE2F,num_rounds,1,2
"#;

    #[test]
    fn test_parse_csv() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();

        // Check opcodes
        assert_eq!(table.opcode_count(), 3); // ADDMOD, DIV, KECCAK256

        // DIV: 5 -> 15
        let div_pricing = table.get_opcode_pricing(0x04).unwrap();
        assert_eq!(div_pricing.current_constant, 5);
        assert_eq!(div_pricing.new_constant, 15);
        assert_eq!(div_pricing.new_constant_gas(), 15);

        // KECCAK256 with variable
        let keccak_pricing = table.get_opcode_pricing(OPCODE_KECCAK256).unwrap();
        assert_eq!(keccak_pricing.current_constant, 30);
        assert_eq!(keccak_pricing.new_constant, 45);
        assert_eq!(keccak_pricing.current_variable, Some(6));
        assert_eq!(keccak_pricing.new_variable, Some(6));
        assert_eq!(keccak_pricing.new_constant_gas(), 45);
        assert_eq!(keccak_pricing.new_variable_gas(10), 60); // 10 * 6

        // Precompiles
        assert_eq!(table.precompile_count(), 2); // ECPAIRING, BLAKE2F

        // ECPAIRING
        let ecpairing_pricing =
            table.get_precompile_pricing(&precompile_addresses::ECPAIRING).unwrap();
        assert_eq!(ecpairing_pricing.current_constant, 45000);
        assert_eq!(ecpairing_pricing.new_constant, 45000);
        assert_eq!(ecpairing_pricing.current_variable, Some(34000));
        assert_eq!(ecpairing_pricing.new_variable, Some(34103));
        // For 3 pairs: 45000 + 3 * 34103 = 147309
        assert_eq!(ecpairing_pricing.total_new_gas(3), 147309);

        // BLAKE2F
        let blake2f_pricing = table.get_precompile_pricing(&precompile_addresses::BLAKE2F).unwrap();
        assert_eq!(blake2f_pricing.current_constant, 0);
        assert_eq!(blake2f_pricing.new_constant, 170);
        assert_eq!(blake2f_pricing.current_variable, Some(1));
        assert_eq!(blake2f_pricing.new_variable, Some(2));
        // For 100 rounds: 170 + 100 * 2 = 370
        assert_eq!(blake2f_pricing.total_new_gas(100), 370);
    }

    #[test]
    fn test_operation_pricing_calculations() {
        let pricing = OperationPricing {
            current_constant: 100,
            new_constant: 150,
            current_variable: Some(10),
            new_variable: Some(15),
            variable_type: Some("units".to_string()),
        };

        assert_eq!(pricing.new_constant_gas(), 150);
        assert_eq!(pricing.new_variable_gas(20), 300); // 20 * 15
        assert_eq!(pricing.total_new_gas(20), 450); // 150 + 300
    }

    #[test]
    fn test_no_variable_cost() {
        let pricing = OperationPricing {
            current_constant: 100,
            new_constant: 50,
            current_variable: None,
            new_variable: None,
            variable_type: None,
        };

        assert_eq!(pricing.new_constant_gas(), 50);
        assert_eq!(pricing.new_variable_gas(20), 0); // No variable cost defined
        assert_eq!(pricing.total_new_gas(20), 50);
    }

    #[test]
    fn test_opcode_byte_mapping() {
        assert_eq!(opcode_name_to_byte("DIV"), Some(0x04));
        assert_eq!(opcode_name_to_byte("SDIV"), Some(0x05));
        assert_eq!(opcode_name_to_byte("MOD"), Some(0x06));
        assert_eq!(opcode_name_to_byte("SMOD"), Some(0x07));
        assert_eq!(opcode_name_to_byte("ADDMOD"), Some(0x08));
        assert_eq!(opcode_name_to_byte("MULMOD"), Some(0x09));
        assert_eq!(opcode_name_to_byte("KECCAK256"), Some(0x20));
        assert_eq!(opcode_name_to_byte("SHA3"), Some(0x20));
        assert_eq!(opcode_name_to_byte("UNKNOWN"), None);
    }

    #[test]
    fn test_precompile_address_mapping() {
        let ecrecover = precompile_name_to_address("ECRECOVER").unwrap();
        assert_eq!(ecrecover, precompile_addresses::ECRECOVER);

        let ecpairing = precompile_name_to_address("ECPAIRING").unwrap();
        assert_eq!(ecpairing, precompile_addresses::ECPAIRING);
    }

    #[test]
    fn test_empty_table() {
        let table = GasPricingTable::new();
        assert_eq!(table.opcode_count(), 0);
        assert_eq!(table.precompile_count(), 0);
        assert!(table.get_opcode_pricing(0x04).is_none());
    }
}
