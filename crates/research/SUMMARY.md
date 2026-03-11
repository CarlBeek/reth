# Gas Research Mode Summary

**Status:** experimental

## Implemented

- CLI parsing for `--research.eip2780`, `--research.csv`, `--research.multiplier`,
  `--research.db-path`, and `--research.start-block`
- Multi-schedule replay through the `reth-research` ExEx
- Isolated per-schedule block state
- SQLite persistence for schedule divergences
- Intrinsic repricing support
- CSV opcode / precompile repricing support
- Uniform execution multiplier support
- Divergence classification for status, gas pattern, call tree, and event log differences

## Not Implemented In The Live Path

- state-root or full state-diff persistence
- mempool / builder / transaction-substitution modeling
- cross-client corroboration
- a claim of production readiness

## Current Confidence Level

Useful for exploratory replay and hypothesis generation. Not sufficient by itself for protocol
governance decisions.
