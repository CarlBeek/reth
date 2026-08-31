//! Shared helpers for gas-schedule definitions.
//!
//! Schedules that replay a transaction under a different `SpecId` need to drive
//! revm's native intrinsic-gas calculation for that spec, off the same
//! [`GasParams`] table the EVM will charge against. Both the table builder and
//! the intrinsic driver live here so the per-schedule files stay thin.

use super::context::TxContext;
use revm::{
    context_interface::cfg::{
        gas::InitialAndFloorGas,
        gas_params::{Eip2780TxInfo, GasId, GasParams},
    },
    primitives::hardfork::SpecId,
};

/// Build a fresh [`GasParams`] for `spec` with the given per-slot overrides
/// applied on top of the spec defaults.
///
/// The overrides are applied in order; later entries for the same `GasId` win.
/// Pass an empty slice for the unmodified spec table.
pub fn gas_params_for(spec: SpecId, overrides: &[(GasId, u64)]) -> GasParams {
    let mut params = GasParams::new_spec(spec);
    params.override_gas(overrides.iter().copied());
    params
}

/// Compute the initial/floor gas split for `ctx` under `spec`, using the
/// unmodified spec gas table.
///
/// # EIP-2780
///
/// From Amsterdam the intrinsic base is no longer a flat 21,000 but the
/// decomposed `TX_BASE + to-based + value-based` sum, and the calldata floor is
/// re-anchored on it. revm makes that opt-in per call via
/// [`Eip2780TxInfo`] rather than deriving it from the spec, so the caller has
/// to supply it. The gate here mirrors revm's own
/// (`Cfg::is_amsterdam_eip2780_enabled`, which defaults to "spec is at least
/// Amsterdam"), so a replay lane and the EVM it configures agree on the
/// intrinsic.
pub fn initial_and_floor_gas_for(ctx: &TxContext, spec: SpecId) -> InitialAndFloorGas {
    let eip2780 = spec
        .is_enabled_in(SpecId::AMSTERDAM)
        .then(|| Eip2780TxInfo { value: ctx.value, is_self_transfer: ctx.is_self_transfer() });

    gas_params_for(spec, &[]).initial_tx_gas(
        &ctx.input,
        ctx.is_create,
        ctx.access_list_accounts,
        ctx.access_list_storage_slots,
        ctx.authorization_count,
        eip2780,
    )
}
