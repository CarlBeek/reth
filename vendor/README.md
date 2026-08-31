# Vendored revm crates

`revm-context-interface` and `revm-handler`, pinned at **43.0.0** and substituted
for the published crates through `[patch.crates-io]` in the root `Cargo.toml`.
Both are listed under `workspace.exclude`, so they are built only via that patch.

## Why they are vendored

One reason only: the `state_gas_demanded` diagnostic counter, which has no
upstream equivalent. EIP-8037's `state_gas_spent` counts state gas that was
actually *charged*; a transaction that runs out of gas at a state-creating op
therefore reports 0 state gas, which is the most misleading possible number for
exactly the transactions this research studies. `state_gas_demanded` counts the
attempt instead, so the dataset can say "this op needed N state gas".

It is a pure side channel: it never feeds an affordability decision, never
touches `reservoir` or `state_gas_spent`, saturates rather than overflowing, and
is never rolled back. Nothing about consensus behaviour changes.

Everything else the fork used to patch here — the Glamsterdam intrinsic slots,
the recipient/value gas params, the floor re-anchoring — is now native in revm 43
and has been deleted.

## The patch: 5 hunks in 5 files

| File | What |
| --- | --- |
| `revm-context-interface/src/cfg/gas.rs` | the `GasTracker` field, its accessors, and the increment at the top of `record_state_cost` (the funnel every state charge passes through) |
| `revm-context-interface/src/result.rs` | the `ResultGas` field, getter and `with_*` builder |
| `revm-handler/src/frame.rs` | child→parent propagation in `handle_reservoir_remaining_gas`, above the settle so it survives revert and halt |
| `revm-handler/src/handler.rs` | preserve the tally across the `runtime_oog_result` gas rebuild — the OOG path the counter exists for |
| `revm-handler/src/post_execution.rs` | copy it onto the final `ResultGas` in `build_result_gas` |

`crates/research/tests/state_gas_demanded.rs` pins each link in that chain.

## Keeping it thin

**Do not run `cargo fmt` inside these directories.** They are excluded from the
workspace, so `cargo +nightly fmt --all` correctly skips them; running it here
by hand reformats revm's own code to reth's style and buries the patch in ~1,600
lines of noise. The patch is worth keeping auditable:

```sh
# Should show only the 5 hunks above.
cargo package --list -p revm-handler   # or fetch the .crate from crates.io
diff -r <extracted-crate>/src vendor/revm-handler/src
```

## Upgrading revm

1. Download the new `.crate` for both packages and extract over `vendor/*/src`.
2. Re-apply the five hunks. They sit in stable code paths but the surrounding
   code does move — the 40→43 jump rewrote `handler.rs` enough that hunk 4 had
   to move from `last_frame_result` to `runtime_oog_result`.
3. Check whether any **new** state-gas charge path bypasses `record_state_cost`.
   As of 43 the EIP-2780 runtime phases (`pre_execution::apply_auth_list_eip2780`
   and `execution.rs`'s first-frame charges) all route through it, so the single
   increment covers them.
4. Run `cargo test -p reth-research`, especially `state_gas_demanded` and
   `amsterdam_matches_execution_specs`.
5. If upstream ever adds an equivalent counter, delete this directory, the
   `[patch.crates-io]` entries and the `workspace.exclude` entries.
