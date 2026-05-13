//! EIP-8037: State Creation Gas Cost Increase schedule.
//!
//! Numbers track the latest draft of EIP-8037 (PR ethereum/EIPs#11616:
//! "Update numbers and rationale"). The state-byte parameters are
//! authoritative; the regular-gas constants (`CREATE_ACCESS`,
//! `STORAGE_WRITE`, `ACCOUNT_WRITE`, `REGULAR_PER_AUTH_BASE_COST`) are
//! defined in EIP-8038 and the PR explicitly notes they are "not yet
//! final" — the values here are pre-PR-11616 derivations kept as
//! reasonable placeholders.
//!
//! Implementation notes:
//! - `configure_evm_env` switches the EVM to `SpecId::AMSTERDAM` (which gives us revm's native
//!   EIP-8037 reservoir / state-gas accounting) and then *overrides* the relevant entries of revm's
//!   gas-param table with the PR-11616 constants. revm ships hardcoded `CPSB = 1174`,
//!   `STATE_BYTES_PER_STORAGE_SET = 32`, `STATE_BYTES_PER_NEW_ACCOUNT = 112`, so without the
//!   override the runtime would silently use the older numbers.
//! - `intrinsic_gas` mirrors the same overrides on a fresh `GasParams::new_spec(AMSTERDAM)` and
//!   calls `initial_tx_gas` directly, since the top-level `calculate_initial_tx_gas` helper builds
//!   its own `GasParams` from the spec and there's no hook for injecting overrides.

use super::{
    context::TxContext,
    traits::{GasSchedule, ScheduleKind},
};
use reth_evm::EvmEnv;
use revm::{
    context_interface::{
        cfg::{
            gas::InitialAndFloorGas,
            gas_params::{GasId, GasParams},
        },
        Cfg,
    },
    primitives::hardfork::SpecId,
};

/// Gas constants from the current EIP-8037 draft (PR ethereum/EIPs#11616).
#[derive(Debug, Clone, Copy)]
pub struct Eip8037Constants;

impl Eip8037Constants {
    // ── State-byte parameters (authoritative, from PR 11616) ────────────

    /// Cost per state byte. PR 11616 raised this from 1174 to 1530.
    pub const CPSB: u64 = 1_530;

    /// State bytes charged for one new storage slot. PR 11616 raised
    /// this from 32 to 64.
    pub const STATE_BYTES_PER_STORAGE_SET: u64 = 64;

    /// State bytes charged for one new account. PR 11616 raised this
    /// from 112 to 120.
    pub const STATE_BYTES_PER_NEW_ACCOUNT: u64 = 120;

    /// State bytes charged for an EIP-7702 authorization base. Unchanged
    /// at 23 in PR 11616.
    pub const STATE_BYTES_PER_AUTH_BASE: u64 = 23;

    /// Upper bound on new storage slots a single system call may write.
    /// Matches `MAX_WITHDRAWAL_REQUESTS_PER_BLOCK` (EIP-7002).
    pub const SYSTEM_MAX_SSTORES_PER_CALL: u64 = 16;

    // ── Regular-gas charges (deferred to EIP-8038; values placeholder) ──
    //
    // PR 11616 punts these constants to EIP-8038 and notes "values not
    // yet final". The values we keep here came from the previous
    // EIP-8037 master text and remain reasonable defaults until 8038
    // settles. They're surfaced for the tests / forensics rather than
    // applied at runtime — revm's Amsterdam table covers the regular
    // gas portion natively.

    /// Regular gas for CREATE and create transactions, on top of state gas.
    /// EIP-8038 name: `CREATE_ACCESS`. TBD.
    pub const CREATE_ACCESS: u64 = 9_000;

    /// Regular gas for SSTORE 0 -> non-zero, excluding state gas.
    /// EIP-8038 name: `STORAGE_WRITE`. TBD.
    pub const STORAGE_WRITE: u64 = 2_900;

    /// Regular gas for creating a new account via CALL / CALLCODE /
    /// SELFDESTRUCT. EIP-8038 name: `ACCOUNT_WRITE`. TBD.
    pub const ACCOUNT_WRITE: u64 = 25_000;

    /// Regular gas charged per EIP-7702 authorization base.
    /// EIP-8038 name: `REGULAR_PER_AUTH_BASE_COST`. TBD.
    pub const REGULAR_PER_AUTH_BASE_COST: u64 = 7_500;

    // ── Derived state gas amounts ───────────────────────────────────────

    /// State gas for creating one account.
    pub const NEW_ACCOUNT_STATE_GAS: u64 = Self::STATE_BYTES_PER_NEW_ACCOUNT * Self::CPSB;

    /// State gas for one new storage slot.
    pub const STORAGE_SET_STATE_GAS: u64 = Self::STATE_BYTES_PER_STORAGE_SET * Self::CPSB;

    /// State gas for one EIP-7702 authorization.
    pub const AUTH_STATE_GAS: u64 =
        (Self::STATE_BYTES_PER_NEW_ACCOUNT + Self::STATE_BYTES_PER_AUTH_BASE) * Self::CPSB;
}

/// Apply PR-11616 state-byte constants on top of revm's hardcoded
/// `SpecId::AMSTERDAM` gas-param table.
///
/// revm bakes the older spec numbers (`CPSB = 1174`,
/// `STATE_BYTES_PER_STORAGE_SET = 32`, `STATE_BYTES_PER_NEW_ACCOUNT = 112`)
/// into `with_spec_and_mainnet_gas_params(AMSTERDAM)`. This helper
/// overlays the new constants so both runtime gas charges (via the cfg
/// env) and the intrinsic-gas computation (via a fresh `GasParams`) end
/// up using PR-11616 values.
fn apply_pr11616_overrides(params: &mut GasParams) {
    let cpsb = Eip8037Constants::CPSB;
    let bytes_storage = Eip8037Constants::STATE_BYTES_PER_STORAGE_SET;
    let bytes_account = Eip8037Constants::STATE_BYTES_PER_NEW_ACCOUNT;
    let bytes_auth = Eip8037Constants::STATE_BYTES_PER_AUTH_BASE;

    // Per-opcode state-gas figures. revm reads these via
    // `GasParams::{sstore_set_state_gas,new_account_state_gas,
    // create_state_gas,code_deposit_state_gas}` during execution.
    let storage_set_state = bytes_storage * cpsb;
    let new_account_state = bytes_account * cpsb;

    // SSTORE 0 -> nonzero refund. revm models this as
    //   sstore_set_state_gas + 2800
    // (the 2800 here is the regular-gas portion that drops when the
    //  slot is reset; not a number the PR changes).
    let sstore_set_refund = storage_set_state + 2_800;

    // EIP-7702 authorization bundles regular + state into one entry on
    // revm's table. We reconstruct it the same way revm does so the
    // intrinsic-gas split (regular vs state) stays correct.
    let auth_total_cost =
        Eip8037Constants::REGULAR_PER_AUTH_BASE_COST + (bytes_account + bytes_auth) * cpsb;
    let auth_state_gas = (bytes_account + bytes_auth) * cpsb;
    let auth_refund = bytes_account * cpsb;

    params.override_gas([
        (GasId::sstore_set_state_gas(), storage_set_state),
        (GasId::new_account_state_gas(), new_account_state),
        (GasId::create_state_gas(), new_account_state),
        (GasId::code_deposit_state_gas(), cpsb),
        (GasId::sstore_set_refund(), sstore_set_refund),
        (GasId::tx_eip7702_per_empty_account_cost(), auth_total_cost),
        (GasId::tx_eip7702_per_auth_state_gas(), auth_state_gas),
        (GasId::tx_eip7702_auth_refund(), auth_refund),
    ]);
}

fn pr11616_initial_and_floor_gas(ctx: &TxContext) -> InitialAndFloorGas {
    let mut params = GasParams::new_spec(SpecId::AMSTERDAM);
    apply_pr11616_overrides(&mut params);
    params.initial_tx_gas(
        &ctx.input,
        ctx.is_create,
        ctx.access_list_accounts,
        ctx.access_list_storage_slots,
        ctx.authorization_count,
    )
}

/// EIP-8037 schedule backed by native revm state-gas accounting with
/// PR-11616 constants overlayed on top.
#[derive(Debug, Clone, Copy, Default)]
pub struct Eip8037Schedule;

impl Eip8037Schedule {
    /// Create a new EIP-8037 schedule.
    pub const fn new() -> Self {
        Self
    }
}

impl GasSchedule for Eip8037Schedule {
    fn name(&self) -> &str {
        "eip-8037"
    }

    fn description(&self) -> &str {
        "EIP-8037 PR 11616: static CPSB=1530 state creation gas with reservoir accounting"
    }

    fn config_fingerprint(&self) -> String {
        // Bumped over the previous EIP-8037 numbers (CPSB=1174, etc.):
        // any existing block_coverage rows hashed under the old
        // fingerprint will be treated as stale and re-analysed under the
        // new constants. The fingerprint includes every PR-11616-tracked
        // input so a future numbers-update will trip it again.
        format!(
            "description={}|cpsb={}|storage_bytes={}|account_bytes={}|auth_bytes={}|\
             create_access={}|storage_write={}|account_write={}|regular_per_auth_base={}|\
             native_revm=true|spec=pr-11616",
            self.description(),
            Eip8037Constants::CPSB,
            Eip8037Constants::STATE_BYTES_PER_STORAGE_SET,
            Eip8037Constants::STATE_BYTES_PER_NEW_ACCOUNT,
            Eip8037Constants::STATE_BYTES_PER_AUTH_BASE,
            Eip8037Constants::CREATE_ACCESS,
            Eip8037Constants::STORAGE_WRITE,
            Eip8037Constants::ACCOUNT_WRITE,
            Eip8037Constants::REGULAR_PER_AUTH_BASE_COST,
        )
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::Both
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        // Build a fresh GasParams for Amsterdam, overlay PR-11616
        // numbers, then drive intrinsic-gas through it directly. The
        // top-level helper `calculate_initial_tx_gas` allocates its own
        // GasParams from the spec — no hook for overrides — so we walk
        // the per-instance path instead.
        Some(pr11616_initial_and_floor_gas(ctx).initial_total_gas)
    }

    fn initial_and_floor_gas(&self, ctx: &TxContext) -> Option<InitialAndFloorGas> {
        Some(pr11616_initial_and_floor_gas(ctx))
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        let baseline_max_code_size = env.cfg_env.max_code_size();
        let baseline_max_initcode_size = env.cfg_env.max_initcode_size();

        let mut cfg = env.cfg_env.clone().with_spec_and_mainnet_gas_params(SpecId::AMSTERDAM);
        cfg.limit_contract_code_size = Some(baseline_max_code_size);
        cfg.limit_contract_initcode_size = Some(baseline_max_initcode_size);
        cfg.amsterdam_eip7708_disabled = true;
        cfg.amsterdam_eip7708_delayed_burn_disabled = true;

        // Overlay PR-11616 numbers on top of revm's Amsterdam defaults.
        // Without this, the EVM would silently charge the old CPSB=1174
        // / 32-byte storage / 112-byte account figures at execution.
        apply_pr11616_overrides(&mut cfg.gas_params);

        env.cfg_env = cfg;
        true
    }

    fn uses_native_intrinsic_gas(&self) -> bool {
        true
    }

    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        if ctx.is_create {
            Some("contract_creation".to_string())
        } else if ctx.authorization_count > 0 {
            Some("authorization".to_string())
        } else if ctx.has_value() && ctx.recipient_info.as_ref().is_none_or(|info| !info.exists) {
            Some("transfer_new_account".to_string())
        } else {
            None
        }
    }

    fn affected_opcodes(&self) -> Vec<u8> {
        vec![0x55, 0xF0, 0xF1, 0xF2, 0xF4, 0xF5, 0xFA]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::TxContext;
    use alloy_primitives::{Address, Bytes, U256};
    use reth_evm::EvmEnv;

    #[test]
    fn pr11616_constants_match_spec_text() {
        // Lock the numbers from the PR so a future revm-bump that
        // changes its internal defaults can't silently change ours.
        assert_eq!(Eip8037Constants::CPSB, 1_530);
        assert_eq!(Eip8037Constants::STATE_BYTES_PER_STORAGE_SET, 64);
        assert_eq!(Eip8037Constants::STATE_BYTES_PER_NEW_ACCOUNT, 120);
        assert_eq!(Eip8037Constants::STATE_BYTES_PER_AUTH_BASE, 23);

        // Derived figures the dashboard frequently quotes.
        assert_eq!(Eip8037Constants::NEW_ACCOUNT_STATE_GAS, 120 * 1_530);
        assert_eq!(Eip8037Constants::STORAGE_SET_STATE_GAS, 64 * 1_530);
        assert_eq!(Eip8037Constants::AUTH_STATE_GAS, (120 + 23) * 1_530);
    }

    #[test]
    fn create_intrinsic_gas_uses_pr11616_account_state_gas() {
        let schedule = Eip8037Schedule::new();
        let ctx = TxContext {
            baseline_intrinsic_gas: 53_000,
            sender: Address::repeat_byte(0x01),
            recipient: None,
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 1_000_000,
            is_create: true,
            recipient_info: None,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
        };

        let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();
        // Base tx (21000) + create-tx regular surcharge (revm's Amsterdam
        // handler charges `CREATE_ACCESS` = 9000) + 120 × 1530 state gas.
        let expected =
            21_000 + Eip8037Constants::CREATE_ACCESS + Eip8037Constants::NEW_ACCOUNT_STATE_GAS;
        assert_eq!(intrinsic, expected);
        assert_eq!(
            schedule.initial_and_floor_gas(&ctx).unwrap().initial_state_gas,
            Eip8037Constants::NEW_ACCOUNT_STATE_GAS
        );
    }

    #[test]
    fn authorization_intrinsic_gas_uses_pr11616_auth_state_gas() {
        let schedule = Eip8037Schedule::new();
        let ctx = TxContext {
            baseline_intrinsic_gas: 58_500,
            sender: Address::repeat_byte(0x01),
            recipient: Some(Address::repeat_byte(0x02)),
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 1_000_000,
            is_create: false,
            recipient_info: None,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 1,
        };

        let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();
        // Base tx (21000) + REGULAR_PER_AUTH_BASE_COST (7500) +
        // (120 + 23) × 1530 state gas.
        let expected = 21_000 +
            Eip8037Constants::REGULAR_PER_AUTH_BASE_COST +
            Eip8037Constants::AUTH_STATE_GAS;
        assert_eq!(intrinsic, expected);
        assert_eq!(
            schedule.initial_and_floor_gas(&ctx).unwrap().initial_state_gas,
            Eip8037Constants::AUTH_STATE_GAS
        );
    }

    #[test]
    fn configure_evm_env_overrides_gas_params() {
        let schedule = Eip8037Schedule::new();
        let mut env = EvmEnv::default();
        env.cfg_env.limit_contract_code_size = Some(24_576);
        env.cfg_env.limit_contract_initcode_size = Some(49_152);

        assert!(schedule.configure_evm_env(&mut env));
        assert_eq!(env.cfg_env.spec, SpecId::AMSTERDAM);
        assert!(env.cfg_env.is_amsterdam_eip8037_enabled());
        assert_eq!(env.cfg_env.max_code_size(), 24_576);
        assert_eq!(env.cfg_env.max_initcode_size(), 49_152);

        // The state-gas table entries should reflect PR-11616 numbers,
        // not revm's hardcoded CPSB=1174 / 32 / 112. Read the table
        // directly because the per-opcode accessors take call-site
        // arguments (e.g. `code_deposit_state_gas(len)` multiplies by
        // length) — the raw entry is the right comparison target.
        let table = env.cfg_env.gas_params.table();
        assert_eq!(table[GasId::sstore_set_state_gas().as_usize()], 64 * 1_530);
        assert_eq!(table[GasId::new_account_state_gas().as_usize()], 120 * 1_530);
        assert_eq!(table[GasId::create_state_gas().as_usize()], 120 * 1_530);
        assert_eq!(table[GasId::code_deposit_state_gas().as_usize()], 1_530);
    }

    #[test]
    fn config_fingerprint_changes_when_numbers_change() {
        // Locks the fingerprint string so any silent constant change
        // here breaks the assertion (forcing a deliberate update).
        let fp = Eip8037Schedule::new().config_fingerprint();
        assert!(fp.contains("cpsb=1530"));
        assert!(fp.contains("storage_bytes=64"));
        assert!(fp.contains("account_bytes=120"));
        assert!(fp.contains("spec=pr-11616"));
    }
}
