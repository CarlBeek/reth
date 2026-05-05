//! EIP-2780: Reduced Intrinsic Transaction Gas schedule.
//!
//! This schedule implements category-based intrinsic gas costs as proposed in EIP-2780.
//! Different transaction types have different base costs instead of the uniform 21,000.

use super::{
    context::TxContext,
    traits::{GasSchedule, ScheduleKind},
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Gas constants for EIP-2780.
#[derive(Debug, Clone, Copy)]
pub struct Eip2780Constants;

impl Eip2780Constants {
    /// Base transaction cost (replaces the current 21,000).
    pub const TX_BASE_COST: u64 = 4_500;

    /// Cold account access cost for accounts without code (EOAs).
    pub const COLD_ACCOUNT_COST_NOCODE: u64 = 500;

    /// Cold account access cost for accounts with code (contracts).
    pub const COLD_ACCOUNT_COST_CODE: u64 = 2_600;

    /// State update cost (balance change).
    pub const STATE_UPDATE: u64 = 1_000;

    /// New account creation cost.
    pub const GAS_NEW_ACCOUNT: u64 = 25_000;

    /// Current Ethereum base transaction cost (pre-EIP-2780).
    pub const CURRENT_BASE_COST: u64 = 21_000;

    /// Create transaction base cost (current).
    pub const CURRENT_CREATE_COST: u64 = 53_000;
}

/// Transaction category for EIP-2780 gas cost calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eip2780Category {
    /// No-op transfer to an existing EOA (no value, no code).
    NopToEoa,
    /// No-op transfer to an empty account.
    NopToEmpty,
    /// No-op transfer to self (sender == recipient).
    NopToSelf,
    /// ETH transfer to an existing EOA.
    TransferToEoa,
    /// Call to a contract without ETH transfer.
    CallToContract,
    /// ETH transfer to a contract.
    TransferCallToContract,
    /// ETH transfer creating a new account.
    TransferNewAccount,
    /// Contract creation transaction.
    ContractCreation,
}

impl Eip2780Category {
    /// Classify a transaction based on its context.
    pub fn classify(ctx: &TxContext) -> Self {
        // Contract creation
        if ctx.is_create {
            return Self::ContractCreation;
        }

        // Self-transfer
        if ctx.is_self_transfer() {
            return Self::NopToSelf;
        }

        let has_value = ctx.has_value();

        match &ctx.recipient_info {
            None => {
                // Recipient doesn't exist
                if has_value {
                    Self::TransferNewAccount
                } else {
                    Self::NopToEmpty
                }
            }
            Some(info) => {
                let is_empty = info.is_empty();
                let is_contract = info.is_contract();

                if is_empty {
                    if has_value {
                        Self::TransferNewAccount
                    } else {
                        Self::NopToEmpty
                    }
                } else if is_contract {
                    if has_value {
                        Self::TransferCallToContract
                    } else {
                        Self::CallToContract
                    }
                } else {
                    // EOA with balance/nonce
                    if has_value {
                        Self::TransferToEoa
                    } else {
                        Self::NopToEoa
                    }
                }
            }
        }
    }

    /// Get the base intrinsic gas for this category under EIP-2780.
    pub const fn base_intrinsic_gas(self) -> u64 {
        use Eip2780Constants as C;

        match self {
            Self::NopToEoa | Self::NopToEmpty | Self::NopToSelf => C::TX_BASE_COST,
            Self::TransferToEoa => C::TX_BASE_COST + C::COLD_ACCOUNT_COST_NOCODE + C::STATE_UPDATE,
            Self::CallToContract => C::TX_BASE_COST + C::COLD_ACCOUNT_COST_CODE,
            Self::TransferCallToContract => {
                C::TX_BASE_COST + C::COLD_ACCOUNT_COST_CODE + C::STATE_UPDATE
            }
            Self::TransferNewAccount => {
                C::TX_BASE_COST + C::COLD_ACCOUNT_COST_NOCODE + C::GAS_NEW_ACCOUNT
            }
            Self::ContractCreation => C::CURRENT_CREATE_COST,
        }
    }

    /// Get the current (pre-EIP-2780) intrinsic gas for this category.
    pub const fn current_intrinsic_gas(self) -> u64 {
        match self {
            Self::ContractCreation => Eip2780Constants::CURRENT_CREATE_COST,
            _ => Eip2780Constants::CURRENT_BASE_COST,
        }
    }

    /// Calculate the gas delta (current - eip2780).
    /// Positive means EIP-2780 saves gas.
    pub const fn gas_delta(self) -> i64 {
        self.current_intrinsic_gas() as i64 - self.base_intrinsic_gas() as i64
    }

    /// Check if this is a NOP transaction.
    pub const fn is_nop(self) -> bool {
        matches!(self, Self::NopToEoa | Self::NopToEmpty | Self::NopToSelf)
    }

    /// Check if this involves a contract interaction.
    pub const fn is_contract_interaction(self) -> bool {
        matches!(self, Self::CallToContract | Self::TransferCallToContract | Self::ContractCreation)
    }
}

impl fmt::Display for Eip2780Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NopToEoa => write!(f, "nop_to_eoa"),
            Self::NopToEmpty => write!(f, "nop_to_empty"),
            Self::NopToSelf => write!(f, "nop_to_self"),
            Self::TransferToEoa => write!(f, "transfer_to_eoa"),
            Self::CallToContract => write!(f, "call_to_contract"),
            Self::TransferCallToContract => write!(f, "transfer_call_to_contract"),
            Self::TransferNewAccount => write!(f, "transfer_new_account"),
            Self::ContractCreation => write!(f, "contract_creation"),
        }
    }
}

impl std::str::FromStr for Eip2780Category {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "nop_to_eoa" => Ok(Self::NopToEoa),
            "nop_to_empty" => Ok(Self::NopToEmpty),
            "nop_to_self" => Ok(Self::NopToSelf),
            "transfer_to_eoa" => Ok(Self::TransferToEoa),
            "call_to_contract" => Ok(Self::CallToContract),
            "transfer_call_to_contract" => Ok(Self::TransferCallToContract),
            "transfer_new_account" => Ok(Self::TransferNewAccount),
            "contract_creation" => Ok(Self::ContractCreation),
            _ => Err(format!("Invalid EIP-2780 category: {}", s)),
        }
    }
}

/// EIP-2780 gas schedule: reduced intrinsic gas based on transaction category.
#[derive(Debug, Clone, Default)]
pub struct Eip2780Schedule {
    /// Optional filter for specific categories to analyze.
    /// If None, all categories are analyzed.
    pub categories_filter: Option<Vec<Eip2780Category>>,
}

impl Eip2780Schedule {
    /// Create a new EIP-2780 schedule.
    pub const fn new() -> Self {
        Self { categories_filter: None }
    }

    /// Create a schedule that only analyzes specific categories.
    pub fn with_categories(categories: Vec<Eip2780Category>) -> Self {
        Self { categories_filter: Some(categories) }
    }

    /// Check if a category should be analyzed.
    fn should_analyze(&self, category: Eip2780Category) -> bool {
        match &self.categories_filter {
            None => true,
            Some(filter) => filter.contains(&category),
        }
    }
}

impl GasSchedule for Eip2780Schedule {
    fn name(&self) -> &str {
        "eip-2780"
    }

    fn description(&self) -> &str {
        "EIP-2780: Reduced intrinsic gas based on transaction category"
    }

    fn config_fingerprint(&self) -> String {
        let categories = self
            .categories_filter
            .as_ref()
            .map(|categories| {
                let mut labels: Vec<_> = categories.iter().map(ToString::to_string).collect();
                labels.sort();
                labels.join(",")
            })
            .unwrap_or_else(|| "all".to_string());
        format!("description={}|categories={categories}", self.description())
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::IntrinsicOnly
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        let category = Eip2780Category::classify(ctx);

        if !self.should_analyze(category) {
            return None;
        }

        // Preserve all fork-specific ancillary intrinsic charges (access list,
        // initcode metering, authorization list, calldata repricing, etc.) and
        // only swap out the transaction's base intrinsic component.
        Some(
            ctx.baseline_intrinsic_gas
                .saturating_sub(category.current_intrinsic_gas())
                .saturating_add(category.base_intrinsic_gas()),
        )
    }

    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        Some(Eip2780Category::classify(ctx).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::context::RecipientInfo;
    use alloy_primitives::{Address, Bytes, U256};

    fn make_tx_ctx(
        is_create: bool,
        has_value: bool,
        recipient_info: Option<RecipientInfo>,
    ) -> TxContext {
        let sender = Address::repeat_byte(0x01);
        let recipient = if is_create { None } else { Some(Address::repeat_byte(0x02)) };

        TxContext {
            baseline_intrinsic_gas: if is_create {
                Eip2780Constants::CURRENT_CREATE_COST
            } else {
                Eip2780Constants::CURRENT_BASE_COST
            },
            sender,
            recipient,
            value: if has_value { U256::from(1000) } else { U256::ZERO },
            input: Bytes::new(),
            gas_limit: 100_000,
            is_create,
            recipient_info,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
        }
    }

    #[test]
    fn test_category_classification() {
        // Contract creation
        let ctx = make_tx_ctx(true, false, None);
        assert_eq!(Eip2780Category::classify(&ctx), Eip2780Category::ContractCreation);

        // Transfer to new account
        let ctx = make_tx_ctx(false, true, None);
        assert_eq!(Eip2780Category::classify(&ctx), Eip2780Category::TransferNewAccount);

        // NOP to empty
        let ctx = make_tx_ctx(false, false, None);
        assert_eq!(Eip2780Category::classify(&ctx), Eip2780Category::NopToEmpty);

        // Transfer to EOA
        let ctx = make_tx_ctx(
            false,
            true,
            Some(RecipientInfo {
                exists: true,
                has_code: false,
                balance: U256::from(100),
                nonce: 1,
            }),
        );
        assert_eq!(Eip2780Category::classify(&ctx), Eip2780Category::TransferToEoa);

        // Call to contract
        let ctx = make_tx_ctx(
            false,
            false,
            Some(RecipientInfo { exists: true, has_code: true, balance: U256::ZERO, nonce: 1 }),
        );
        assert_eq!(Eip2780Category::classify(&ctx), Eip2780Category::CallToContract);
    }

    #[test]
    fn test_gas_costs() {
        assert_eq!(Eip2780Category::NopToEoa.base_intrinsic_gas(), 4_500);
        assert_eq!(Eip2780Category::TransferToEoa.base_intrinsic_gas(), 6_000);
        assert_eq!(Eip2780Category::CallToContract.base_intrinsic_gas(), 7_100);
        assert_eq!(Eip2780Category::TransferCallToContract.base_intrinsic_gas(), 8_100);
        assert_eq!(Eip2780Category::TransferNewAccount.base_intrinsic_gas(), 30_000);
    }

    #[test]
    fn test_gas_deltas() {
        assert_eq!(Eip2780Category::NopToEoa.gas_delta(), 16_500);
        assert_eq!(Eip2780Category::TransferToEoa.gas_delta(), 15_000);
        assert_eq!(Eip2780Category::CallToContract.gas_delta(), 13_900);
        assert_eq!(Eip2780Category::TransferNewAccount.gas_delta(), -9_000);
    }

    #[test]
    fn test_schedule_intrinsic_gas() {
        let schedule = Eip2780Schedule::new();

        // Transfer to EOA with 100 bytes of calldata (all non-zero)
        let ctx = TxContext {
            baseline_intrinsic_gas: 22_600,
            sender: Address::repeat_byte(0x01),
            recipient: Some(Address::repeat_byte(0x02)),
            value: U256::from(1000),
            input: Bytes::from(vec![1u8; 100]),
            gas_limit: 100_000,
            is_create: false,
            recipient_info: Some(RecipientInfo {
                exists: true,
                has_code: false,
                balance: U256::from(100),
                nonce: 1,
            }),
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
        };

        let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();
        // Base: 6,000 + Calldata: 100 * 16 = 1,600 = 7,600
        assert_eq!(intrinsic, 7_600);
    }

    #[test]
    fn test_schedule_category_filter() {
        let schedule = Eip2780Schedule::with_categories(vec![Eip2780Category::TransferToEoa]);

        // Transfer to EOA should be analyzed
        let ctx = make_tx_ctx(
            false,
            true,
            Some(RecipientInfo {
                exists: true,
                has_code: false,
                balance: U256::from(100),
                nonce: 1,
            }),
        );
        assert!(schedule.intrinsic_gas(&ctx).is_some());

        // Call to contract should NOT be analyzed (filtered out)
        let ctx = make_tx_ctx(
            false,
            false,
            Some(RecipientInfo { exists: true, has_code: true, balance: U256::ZERO, nonce: 1 }),
        );
        assert!(schedule.intrinsic_gas(&ctx).is_none());
    }

    #[test]
    fn test_schedule_preserves_non_base_intrinsic_charges() {
        let schedule = Eip2780Schedule::new();

        let ctx = TxContext {
            baseline_intrinsic_gas: 24_000,
            sender: Address::repeat_byte(0x01),
            recipient: Some(Address::repeat_byte(0x02)),
            value: U256::from(1000),
            input: Bytes::new(),
            gas_limit: 100_000,
            is_create: false,
            recipient_info: Some(RecipientInfo {
                exists: true,
                has_code: false,
                balance: U256::from(100),
                nonce: 1,
            }),
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
        };

        assert_eq!(schedule.intrinsic_gas(&ctx), Some(9_000));
    }

    #[test]
    fn test_category_display_and_parse() {
        for category in [
            Eip2780Category::NopToEoa,
            Eip2780Category::NopToEmpty,
            Eip2780Category::NopToSelf,
            Eip2780Category::TransferToEoa,
            Eip2780Category::CallToContract,
            Eip2780Category::TransferCallToContract,
            Eip2780Category::TransferNewAccount,
            Eip2780Category::ContractCreation,
        ] {
            let s = category.to_string();
            let parsed: Eip2780Category = s.parse().unwrap();
            assert_eq!(category, parsed);
        }
    }
}
