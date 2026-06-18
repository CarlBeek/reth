//! Named opcode bytes used across the schedule definitions and the inspector.
//!
//! Centralizes the EVM opcode constants that the gas-schedule logic and the
//! [`ScheduleInspector`](crate::multi_schedule_inspector::ScheduleInspector)
//! match on, replacing scattered bare hex literals so the `matches!` / `match`
//! sites read by mnemonic. Values are the canonical EVM opcode encodings.

/// `BALANCE` — account balance lookup (account-access opcode).
pub const BALANCE: u8 = 0x31;
/// `EXTCODESIZE` — external code size (makes a second DB read under EIP-8038).
pub const EXTCODESIZE: u8 = 0x3B;
/// `EXTCODECOPY` — external code copy (makes a second DB read under EIP-8038).
pub const EXTCODECOPY: u8 = 0x3C;
/// `EXTCODEHASH` — external code hash (account-access opcode).
pub const EXTCODEHASH: u8 = 0x3F;
/// `SLOAD` — storage read.
pub const SLOAD: u8 = 0x54;
/// `SSTORE` — storage write.
pub const SSTORE: u8 = 0x55;
/// `GAS` — remaining-gas push (tracked for gas-dependent loop detection).
pub const GAS: u8 = 0x5A;
/// `CREATE` — contract creation.
pub const CREATE: u8 = 0xF0;
/// `CALL` — message call (may transfer value).
pub const CALL: u8 = 0xF1;
/// `CALLCODE` — legacy call with caller's storage (may transfer value).
pub const CALLCODE: u8 = 0xF2;
/// `DELEGATECALL` — call with caller's context (no value transfer).
pub const DELEGATECALL: u8 = 0xF4;
/// `CREATE2` — contract creation at a deterministic address.
pub const CREATE2: u8 = 0xF5;
/// `STATICCALL` — read-only message call (no value transfer).
pub const STATICCALL: u8 = 0xFA;
/// `SELFDESTRUCT` — account destruction (account-access opcode).
pub const SELFDESTRUCT: u8 = 0xFF;
