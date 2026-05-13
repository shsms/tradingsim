# tradingsim

Simulator of a continuous intraday electricity exchange, exposing the
Frequenz Electricity Trading gRPC API. Reference behaviour is 
SPOT's intraday platform for the DE-LU bidding zone, but every market knob
is data-driven so the same binary can stand in for FR / AT / NL / BE
or a synthetic zone.

Sibling project to [`../switchyard`](../switchyard); same operating
model: Rust owns deterministic machinery, tulisp wires + animates the
world, axum SPA + REPL provide a UI, `tsctl` mirrors `swctl`.

## Canonical roadmap

[`plan.org`](plan.org) is authoritative for scope, market model, lisp
DSL design, the 11-RPC gRPC mapping, UI shape, scenarios, testing
strategy, and the 12-phase roadmap. Use it to figure out what to
build; use this file to figure out how to work in the repo.

## Layout

This is early — most of plan.org is unbuilt. Current shape:

- `src/lib.rs` — module roots
- `src/proto.rs` — tonic include of the generated proto; re-exports
  `proto::common` = `frequenz.api.common.v1alpha8` and
  `proto::trading` = `frequenz.api.electricity_trading.electricity_trading.v1`
- `src/sim/decimal.rs` — `snap_to_tick`, `is_multiple_of`, default
  tick / step constants
- `src/sim/market.rs` — `Area`, `Currency`, `DeliveryDuration`,
  `DeliveryPeriod`, `MarketRules`, `MarketRegistry`
- `src/bin/tradingsim.rs` — stub server entry (logs and exits)
- `src/bin/tsctl.rs` — stub client (clap scaffolding)

Target layout in plan.org §Architecture overview.

## Build / run / test

```sh
cargo build
cargo test                                # unit tests
cargo run --bin tradingsim                # stub, exits
cargo run --bin tsctl -- --help           # stub
```

The server (when wired in Phase 4) defaults to `[::1]:8810` for gRPC
and `127.0.0.1:8811` for the UI.

## Proto submodule

`submodules/frequenz-api-electricity-trading` is pinned at `3a41f88`;
its nested `frequenz-api-common` submodule pins `fc70cb9`. `build.rs`
points `tonic-prost-build` at both. The env var
`TRADINGSIM_PROTO_ROOT` overrides the submodule path for downstream
packagers.

After a fresh clone:

```sh
git submodule update --init --recursive
```

## Python smoke client

[`frequenz-client-electricity-trading-python`](https://github.com/frequenz-floss/frequenz-client-electricity-trading-python)
is the official client for this API. Use it as the end-to-end
smoke-test driver once Phase 4 lands the gRPC server: point it at
`[::1]:8810` and exercise place / list / cancel + the four stream
RPCs.

## Commit conventions

Per the cross-project commit style: imperative subject, no prefix tag,
no AI co-author footer. Commits stay around 100 lines following the
introduce → rewire → remove → cleanup pattern. The plan.org roadmap
already breaks each phase into ~commit-sized chunks.

## Adding a sim type (current pattern, expanding)

Each new `sim::*` module:

1. Pure data + small helpers, no I/O, no async.
2. Inline `#[cfg(test)] mod tests` covering the invariants that
   downstream code (matcher, validator, proto_conv) will rely on.
3. Re-export from `src/sim/mod.rs`.
4. Proto bridging lives in `src/proto_conv.rs` (Phase 2, last
   commit) — not in the type's own module.

## Dependencies

- `tonic = "0.14"` stack (tonic, tonic-prost, prost, prost-types) plus
  `tonic-prost-build` in `[build-dependencies]`. Don't downgrade
  major.
- `tulisp`, `tulisp-async`, `tulisp-fmt` — same pinned git revisions
  as switchyard. They follow tulisp's `fmt` branch for AsPlist!,
  etags, and async timer primitives.
- `rust_decimal` with the `macros` feature for `dec!`. The proto
  `Decimal` is a stringly-typed message; we round-trip via
  `Decimal::from_str` / `Decimal::to_string`.
- `chrono` with `serde` for `DateTime<Utc>` (delivery periods,
  timestamps).
- `axum = "0.8"` with `ws` for the UI, `rust-embed` for SPA assets,
  `reqwest` for `tsctl`'s HTTP calls into the UI server's scenario
  endpoints.
