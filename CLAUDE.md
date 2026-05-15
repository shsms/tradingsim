# tradingsim

Simulator of a continuous intraday electricity exchange, exposing the
Frequenz Electricity Trading gRPC API. Ships configured for the four
German TSO control zones (TN / AM / HZ / BW) with four neighbouring
international zones (FR / NL / BE / AT) for cross-border tests, but
every market knob is data-driven so the same binary can stand in for
any zone set or a synthetic configuration.

Sibling project to [`../switchyard`](../switchyard); same operating
model: Rust owns deterministic machinery, tulisp wires + animates the
world, axum SPA + REPL provide a UI, `tsctl` mirrors `swctl`.

## Canonical roadmap

[`todo.org`](todo.org) tracks pending work. Use this file (CLAUDE.md)
to figure out how to work in the repo; use todo.org to figure out
what to work on next.

## Layout

Phases 0–10 + hot reload all functional. Scope per user request:
LIMIT orders only, 15-min delivery duration only.

- `src/lib.rs` — module roots
- `src/proto.rs` — tonic include of the generated proto
- `src/proto_conv.rs` — bidirectional sim ↔ proto bridges
- `src/server.rs` — `ElectricityTradingServer` (impls all 11 RPCs;
  Create / Get / Cancel / CancelAll / List / Receive…OrdersStream /
  ListTrades / Receive…TradesStream / ReceivePublicTradesStream
  wired; Update + ReceivePublicOrderBookStream still unimplemented)
- `src/sim/decimal.rs` — `snap_to_tick`, `is_multiple_of`, defaults
- `src/sim/market.rs` — `Area`, `Currency`, `MarketRules`,
  `MarketRegistry`, `DeliveryDuration/Period`
- `src/sim/order.rs` — `Order`, `OrderDetail`, `StateDetail`, all
  state-machine enums
- `src/sim/trade.rs` — `Trade`, `PublicTrade`, `TradeState`
- `src/sim/book.rs` — `OrderBook` (price-keyed FIFO half-books)
- `src/sim/matching.rs` — `match_limit` continuous matcher +
  proptest invariants
- `src/sim/gridpool.rs` — `Gridpool` per-portfolio order/trade index
- `src/sim/world.rs` — owns markets, gridpools, books, broadcasters
  (orders + per-gridpool trades + public trades), id sources;
  `submit_order` / `cancel_order` for gridpool flow and
  `submit_counterparty_order` / `cancel_counterparty_order` for
  synthetic liquidity
- `src/sim/counterparty.rs` — `MarketMaker` + `Aggressor` engines:
  mean-reverting reference with random walk + follow-last-trade pull,
  IOC-clamped aggressor fire. `MmFleetParams` / `AggressorFleetParams`
  carry the band tables the FleetManager re-reads each refresh to
  apply the contract's current offset-to-gate.
- `src/sim/fleet.rs` — `FleetManager`: per-contract counterparty
  lifecycle. Spawns one MM (and N aggressors) per delivery contract
  in each fleet's rolling window, rotates them on every quarter
  boundary (gating ones retired, new far-edge spawned), and exposes
  `mm_views()` / `aggressor_views()` for the bias tick. Replaces the
  old slot-indexed `spawn_mm_task` / `spawn_aggressor_task` and
  fixes the quarter-boundary price-dip bug (continuous state used
  to leak across rotations).
- `src/lisp/mod.rs` — `Config::new(path)` evaluates a tulisp file
  against runtime defuns: `(set-trading-addr STR)`,
  `(set-ui-addr STR)`, `(set-weather-addr STR)`,
  `(set-physics-tick-ms N)`, `(%make-market …)`,
  `(%make-gridpool …)`, `(%make-coupling …)`,
  `(%make-mm-fleet …)`, `(%make-aggressor-fleet …)`,
  `(define-scenario …)`, `(%make-weather-location …)`,
  `(set-mm-bias-scale F)`, `(set-forward-curve-base H P)`,
  plus tulisp-async's `(run-with-timer …)` + sugar `(every …)`
  from `sim/common.lisp`. `spawn_timer_loop` drives the firing
- `config.lisp` — sample top-level config (4 TSO zones + 4 neighbours,
  cross-border couplings, weather locations, canned scenarios)
- `src/bin/tradingsim.rs` — loads `config.lisp` if present (registers
  fleets from it); falls back to a single default `MmFleetSpec` so
  `tsctl place` has something to trade against on a fresh checkout;
  serves the gRPC API on the configured socket addr
- `src/bin/tsctl.rs` — info / place / get / modify / cancel /
  cancel-all / orders [--live] / trades [--live] / public-trades /
  public-book / scenarios (list / start / next / prev / jump / stop).
  --start accepts "next", "+N", or RFC-3339. The scenarios
  subcommands hit the HTTP UI server on --ui-addr (default 8811);
  weather uses --weather-addr (default [::1]:8820); everything else
  uses the gRPC --addr (default [::1]:8810).
- `tests/grpc_e2e.rs` — out-of-process round-trips against the live
  service

All phases through hot reload are functional; remaining work lives
in [`todo.org`](todo.org).

## Build / run / test

```sh
cargo build
cargo test                                # unit tests
cargo run --bin tradingsim                # stub, exits
cargo run --bin tsctl -- --help           # stub
```

The server defaults to `[::1]:8810` for the trading gRPC,
`127.0.0.1:8811` for the UI, and `[::1]:8820` for the weather
forecast gRPC. All three are configurable via the lisp defuns
`(set-trading-addr)`, `(set-ui-addr)`, `(set-weather-addr)`.

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

## Browser tests (Selenium + headless Firefox)

The fast UI tests in `tests/ui_e2e.rs` drive the axum Router via
`tower::ServiceExt::oneshot` — no browser, no JS. For tests that
need to exercise the live HTML / CSS / JS (layout, drill-downs,
WS updates), use Selenium against headless Firefox:

```sh
/usr/bin/python3 tests/ui_selenium.py          # one-shot
```

Run with `/usr/bin/python3` directly so the system
`python3-selenium` from `/usr/lib/python3/dist-packages` is picked
up — the project venv at `/vagrant/venv` masks `dist-packages` if
its `python3` is used. `geckodriver` is at `/usr/local/bin/`
(installed manually from Mozilla's GitHub releases — Debian does
not package it). Firefox is the `firefox-esr` apt package. The
scripts launch `firefox-esr -headless`, so no display server is
needed.

## Commit conventions

Per the cross-project commit style: imperative subject, no prefix tag,
no AI co-author footer. Commits stay around 100 lines following the
introduce → rewire → remove → cleanup pattern.

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
