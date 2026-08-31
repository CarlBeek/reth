//! Shared helpers for gas-schedule definitions.
//!
//! Schedules that reprice gas by overlaying values on revm's per-spec
//! [`GasParams`] table (e.g. [`Eip8037Schedule`](super::Eip8037Schedule) and
//! [`Eip8038Schedule`](super::Eip8038Schedule)) share two needs: build a
//! `GasParams` for a spec with a set of slot overrides applied, and drive the
//! native intrinsic-gas calculation through that overridden table. Both are
//! provided here so the per-schedule files only carry their own constant tables.

use super::context::TxContext;
use revm::{
    context_interface::cfg::{
        gas::InitialAndFloorGas,
        gas_params::{GasId, GasParams},
    },
    primitives::hardfork::SpecId,
};

/// Build a fresh [`GasParams`] for `spec` with the given per-slot overrides
/// applied on top of the spec defaults.
///
/// The overrides are applied in order; later entries for the same `GasId` win.
pub fn gas_params_for(spec: SpecId, overrides: &[(GasId, u64)]) -> GasParams {
    let mut params = GasParams::new_spec(spec);
    params.override_gas(overrides.iter().copied());
    params
}

/// Compute the initial/floor gas split for `ctx` under `spec` with per-slot
/// overrides applied to the gas-param table.
///
/// This mirrors revm's own `initial_tx_gas` path but lets a schedule inject
/// overridden intrinsic-cost slots (e.g. access-list or create-transaction
/// costs) that the top-level `calculate_initial_tx_gas` helper has no hook for.
///
/// The recipient-side terms (self-transfer exemption, value cost) are read off
/// `ctx` and forwarded, so a schedule that overlays the Glamsterdam
/// `tx_recipient_access_cost` / `tx_value_cost` slots gets the spec's
/// `recipient_execution_gas`. They are inert for schedules that leave those
/// slots at their spec defaults of zero.
pub fn initial_and_floor_gas_for(
    ctx: &TxContext,
    spec: SpecId,
    overrides: &[(GasId, u64)],
) -> InitialAndFloorGas {
    gas_params_for(spec, overrides).initial_tx_gas_with_recipient(
        &ctx.input,
        ctx.is_create,
        ctx.access_list_accounts,
        ctx.access_list_storage_slots,
        ctx.authorization_count,
        ctx.is_self_transfer(),
        ctx.has_value(),
    )
}
