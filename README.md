# tradingsim

A drop-in test target for trading applications that speak the
[Frequenz Electricity Trading gRPC API](https://github.com/frequenz-floss/frequenz-api-electricity-trading).
Point your app at it and exercise live order placement, fills,
streaming order/trade updates, market suspension, weather-driven
price moves, and pre-canned market scenarios — without touching a
real exchange.

What you get out of the box:

- A live, filling continuous-trading book on 15-minute contracts.
- Synthetic market-makers and aggressors that quote and trade
  against your orders.
- A sibling **Weather Forecast gRPC service** your app can also
  subscribe to.
- **Scenarios** — named, scriptable perturbations (scarcity spike,
  buy-only flow, weather shift, sunny holiday, scheduled
  curtailment, …) you can fire from a browser, the CLI, or
  programmatically.
- **Hot reload**: tweak the simulator's behaviour in a tulisp
  config file, save, and the running server picks it up.
- A browser UI for eyeballing what your app is seeing.
- A `tsctl` companion CLI for manual probing.

## Table of contents

- [Quick start](#quick-start)
- [Pointing your trading app at it](#pointing-your-trading-app-at-it)
- [What's simulated](#whats-simulated)
- [Driving behaviour with scenarios](#driving-behaviour-with-scenarios)
- [Fault injection / edge cases](#fault-injection--edge-cases)
- [Configuring the simulator](#configuring-the-simulator)
- [The `tsctl` CLI](#the-tsctl-cli)
- [The browser UI](#the-browser-ui)
- [Constraints worth knowing about](#constraints-worth-knowing-about)
- [Development](#development)

## Quick start

Prerequisites: Rust stable (edition 2024), `git`, and a checkout
with the submodules initialised — the proto definitions live in
`submodules/frequenz-api-electricity-trading`.

```sh
git clone <repo> tradingsim
cd tradingsim
git submodule update --init --recursive

# Browser UI bundle. Skip if you only need the gRPC services — the
# host binary still builds, the SPA routes just 404. `trunk` is a
# separate cargo binary: `cargo install --locked trunk`.
(cd web && trunk build --release)

cargo build --release
./target/release/tradingsim
```

The binary loads `config.lisp` from the current directory and
binds two sockets:

| Service                            | Default address      | Lisp defun                        |
| ---------------------------------- | -------------------- | --------------------------------- |
| gRPC (ElectricityTrading + Weather)| `[::1]:8810`         | `(set-grpc-socket-addr "…")`      |
| Browser UI (HTTP)                  | `127.0.0.1:8811`     | `(set-ui-addr "…")`               |

Both gRPC services live on one socket — tonic routes by service
path. Clients of either API connect to the same address.

Open <http://127.0.0.1:8811/> to see the live order book, trade
tape, weather panel, and scenario controls.

## Pointing your trading app at it

Use whatever client library you already have for the Frequenz
Electricity Trading API. The official Python client is
[frequenz-client-electricity-trading-python](https://github.com/frequenz-floss/frequenz-client-electricity-trading-python);
the proto is the same one the simulator speaks, so any compliant
gRPC client works.

A minimal smoke test from Python:

```python
import grpc
from frequenz.client.electricity_trading import Client

client = Client.connect("grpc://[::1]:8810")
order = await client.create_gridpool_order(
    gridpool_id=1,
    delivery_area="10YDE-EON------1",   # one of the four TSO zones
    delivery_period_start=...,
    side="BUY",
    price=85.00,
    quantity=1.0,
)
async for update in client.receive_gridpool_orders_stream(gridpool_id=1):
    print(update)
```

All 11 RPCs of the Electricity Trading service are implemented:

- `CreateGridpoolOrder` / `GetGridpoolOrder` / `UpdateGridpoolOrder`
- `CancelGridpoolOrder` / `CancelAllGridpoolOrders`
- `ListGridpoolOrders` / `ListGridpoolTrades`
- `ReceiveGridpoolOrdersStream` / `ReceiveGridpoolTradesStream`
- `ReceivePublicTradesStream` / `ReceivePublicOrderBookStream`

A `ReceivePublicTradesStream` subscription whose filter pins a
specific `delivery_period` is closed server-side 15 minutes past
that period's start — the proto's "Replay Window Semantics" let
the service close a stream once its filter selects a gated
contract. Subscriptions with no `delivery_period` filter are the
unbounded market-wide tape.

The Weather Forecast service implements
`ReceiveLiveWeatherForecast` + `ReceiveHistoricalWeatherForecast`,
each frame carrying 24 hourly forecasts (solar, 100 m u/v wind
components, 2 m temperature) per registered weather location.

## What's simulated

Four control zones ship configured out of the box, plus four
neighbouring international zones with cross-border coupling.
Default gridpool `1` covers them all.

The market itself runs continuous price-time-priority matching on
15-minute contracts (sized in 0.1 MW steps, priced in 0.01 EUR
ticks, negative prices admitted as in the real intraday markets
they emulate). The matcher supports `LIMIT` orders with `IOC` and
`FOK` execution options; `AON` and the more exotic order types
are deferred.

Synthetic counterparties cover every contract in the trading
window. They're declared as *fleets* (one recipe per area); the
runtime spawns one market-maker + N aggressors per delivery
contract and rotates them on each quarter boundary — gated
contracts retire, fresh contracts spawn at the far edge.

- **Market-makers** quote bid/ask pairs around a forward-curve
  reference price, with a mean-reverting random walk + a
  follow-last-trade pull so prices drift visibly under flow.
  Quote size and half-spread come from the fleet's band tables
  indexed by the contract's current offset to gate, so a
  contract tightens its spread and grows its depth as it ages
  forward — back-loaded liquidity the way real intraday markets
  show it.
- **Aggressors** fire marketable orders on a horizon-scaled
  cadence — volume back-loads toward gate close the way real
  intraday curves do.

Both react to weather (cloud cover, wind speed, temperature)
through the same forward curve, so wind-driven price drops and
solar-belly dips happen naturally. Weather state is exposed
through the gRPC service for your app to consume.

## Driving behaviour with scenarios

A *scenario* is a named, staged perturbation that biases
counterparty behaviour over a day. Useful for putting your
trading app through specific market regimes without hand-tweaking
knobs.

Canned scenarios shipped in `scenarios/`:

| Scenario              | What it does                                                |
| --------------------- | ----------------------------------------------------------- |
| `sunny-summer-day`    | Midday solar belly drops prices; evening peak recovers      |
| `rainy-summer-day`    | Cloud cover blunts the belly; load stays high               |
| `sunny-summer-holiday` | Lower baseline load, deeper midday belly                   |
| `winter-weekday`      | High morning + evening peaks, cold load                     |
| `windy-winter-night`  | Overnight wind drives prices down                           |
| `scarcity-spike`      | Evening goes parabolic — pinned high bias, runaway prices   |
| `buy-only-flow`       | All aggressor flow on the bid side; sellers vanish          |
| `weather-shift`       | Mid-day forecast revision: cloud drops, solar jumps         |
| `day-ahead-print`     | Sets a previous-day cleared anchor at 12:00 UTC             |

Activate from the UI's Scenarios panel, from the CLI, or via the
HTTP endpoints:

```sh
tsctl scenarios list
tsctl scenarios start scarcity-spike
tsctl scenarios next  scarcity-spike    # jump to next stage
tsctl scenarios stop  scarcity-spike
```

Or write your own (`scenarios/foo.lisp`):

```lisp
(define-scenario
 :name "morning-ramp"
 :description "Cold winter morning waking up"
 :date "2026-01-15"
 :stages
 '((:name "00:00-05:00 quiet"    :hour-from 0  :hour-to 5  :bias-from 0.50 :bias-to 0.50)
   (:name "05:00-08:00 ramp"     :hour-from 5  :hour-to 8  :bias-from 0.55 :bias-to 0.75)
   (:name "08:00-10:00 peak"     :hour-from 8  :hour-to 10 :bias-from 0.75 :bias-to 0.65)
   (:name "10:00-24:00 normal"   :hour-from 10 :hour-to 24 :bias-from 0.50 :bias-to 0.50)))
```

Stage hours are in the configured timezone (Europe/Berlin by
default). The UI's header chip toggles display between local and
UTC; underlying scenario hours stay local.

## Fault injection / edge cases

Useful primitives for exercising your app's error handling:

- **Market suspension.** `(suspend-market)` in lisp (or via
  callback inside a scenario) makes every order submission return
  `FailedPrecondition`. `(resume-market)` clears it. Apps should
  surface this and back off.
- **TSO recall.** `(recall-order ID)` force-cancels a specific
  resting order with `actor = System` and reason `Recall` —
  exercises the "your order disappeared and it wasn't you" path.
- **Self-trade prevention.** Gridpools default to
  `:self-trade-policy "reject"` so a buy that would fill against
  your own sell is rejected with `FailedPrecondition` and
  `SelfTradeRejected`. Opt back into permissive crossing with
  `:self-trade-policy "allow"`.
- **Gate closure.** Submitting at or after a delivery period's
  start returns `GateClosed`. Useful for hitting the deadline.
- **Negative prices.** Real intraday markets admit them under
  supply gluts; the simulator follows suit. Any bias < 0.5 +
  enough scale will push prices below zero. Make sure your app
  doesn't filter on `price > 0`.
- **Cross-border capacity.** Couplings between zones can carry an
  optional `:capacity-mw` cap — exhaust it and further
  cross-area matching stops on that edge.
- **Stream lag.** Tonic's broadcast streams will emit a
  `Lagged(N)` if your client falls behind. The simulator's
  capacity is generous, but if you want to exercise the path,
  pause the client between subscribe and first read.
- **Weather forecast revision.** The
  `weather-shift` scenario flips cloud cover mid-day so the next
  forecast frame your app receives carries materially different
  numbers from the previous one.

## Configuring the simulator

Configuration is a [tulisp](https://github.com/shsms/tulisp) file
the binary evaluates at startup. Save the file and the running
server picks up changes on the next tick — timers and running
streams survive the reload.

A minimal config:

```lisp
(unless (boundp 'tradingsim-loaded)
  (setq tradingsim-loaded t)
  (load "sim/common.lisp"))

(reset-state)                          ;; clear timers from previous load

(set-grpc-socket-addr "[::1]:8810")
(set-ui-addr "127.0.0.1:8811")
(set-physics-tick-ms 100)
(set-timezone "Europe/Berlin")

(register-markets '("10YDE-EON------1"))
(%make-gridpool :id 1 :name "default"
                :areas '("10YDE-EON------1"))

(mm-fleet         :area "10YDE-EON------1" :prefix "tn")  ;; 48 MMs
(aggressor-fleet  :area "10YDE-EON------1" :prefix "tn")  ;; flow
```

`config.lisp` in the repo root shows a fully populated example
with four control zones, four neighbouring zones, cross-border
couplings, weather locations, and the canned scenario library
loaded.

### Most useful defuns

**Transport, clock**

| Defun                                    | Effect                                            |
| ---------------------------------------- | ------------------------------------------------- |
| `(set-grpc-socket-addr "[::1]:8810")`    | trading + weather gRPC bind (both on one socket)  |
| `(set-ui-addr "127.0.0.1:8811")`         | UI HTTP bind                                      |
| `(set-physics-tick-ms 100)`              | matcher loop cadence (ms)                         |
| `(set-timezone "Europe/Berlin")`         | timezone the physics + scenario hours run in     |
| `(set-weather-stream-cadence-seconds N)` | how often `ReceiveLiveWeatherForecast` emits     |

**Markets, gridpools, couplings**

| Defun                                                     | Effect                              |
| --------------------------------------------------------- | ----------------------------------- |
| `(%make-market :area EIC :currency "eur")`                | register a market                   |
| `(register-markets '(EIC ...))`                           | sugar: register many at once        |
| `(%make-gridpool :id N :name S :areas '(...) :self-trade-policy "reject")` | register a portfolio |
| `(%make-coupling :areas '(A B) :gate-offset-seconds N :capacity-mw MW)` | couple two areas + optional cap |
| `(couple-all-pairs '(...))`                               | K_n graph couple                    |
| `(couple-pairs-across '(...) '(...))`                     | bipartite couple                    |

**Counterparty liquidity**

| Defun                                                                                | Effect                                                                       |
| ------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------- |
| `(%make-mm-fleet :name S :area EIC :window-quarters N :size-bands '(...) ...)`       | register one MM fleet — FleetManager spawns one MM per contract in the window |
| `(mm-fleet :area EIC :prefix S)`                                                     | sugar: 48 MMs covering the next 12 hours                                     |
| `(%make-aggressor-fleet :name S :area EIC :profile-sizes '(...) :profile-rate-bases '(...) ...)` | register one aggressor fleet — one taker per (contract, profile) pair        |
| `(aggressor-fleet :area EIC :prefix S)`                                              | sugar: 4 profiles × 48 quarters                                              |
| `(set-mm-bias-scale 25.0)`                                                           | EUR per (bias - 0.5) tilt unit (writes the fleet's demand/surplus shift)     |

Hot reload: re-firing a `(%make-*-fleet …)` call with the same
`:name` updates the fleet's shared params in place — running
per-contract counterparties pick up new bands / rates on their
next tick. Per-MM tuning by name isn't supported (contracts
rotate through bands, so a stable handle would address a moving
target); use the scenarios layer for time-varying bias and the
forward curve / weather knobs for fundamentals.

**Weather**

| Defun                                                                          | Effect                              |
| ------------------------------------------------------------------------------ | ----------------------------------- |
| `(%make-weather-location :name S :area EIC :lat F :lon F :cloud-cover F :mean-wind F)` | register an atmospheric anchor      |
| `(set-weather-cloud-cover F)` / `-mean-wind` / `-direction` / `-temperature-base` | mutate the default location         |

**Pricing**

| Defun                                  | Effect                                              |
| -------------------------------------- | --------------------------------------------------- |
| `(set-forward-curve-base HOUR PRICE)`  | override the per-hour curve anchor                  |

**Market controls**

| Defun                  | Effect                                              |
| ---------------------- | --------------------------------------------------- |
| `(suspend-market)`     | reject all submissions until resume                 |
| `(resume-market)`      | clear the suspension                                |
| `(recall-order ID)`    | TSO recall: force-cancel an order with actor=System |

**Timers + reload**

| Defun                                       | Effect                                                          |
| ------------------------------------------- | --------------------------------------------------------------- |
| `(every :milliseconds N :call (lambda () ...))` | periodic callback (sugar over `run-with-timer`)             |
| `(run-with-timer FIRST REPEAT FN)`          | tulisp-async primitive                                          |
| `(reset-state)`                             | cancel all timers; call at the top of `config.lisp` for hot reload |
| `(watch-file PATH)`                         | also reload when this support file changes                      |

## The `tsctl` CLI

```
tsctl <command> [args]

  info           show endpoint info
  place          place a LIMIT order
  get            fetch a single order
  modify         modify a resting order
  cancel         cancel one order
  cancel-all     cancel everything on a gridpool
  orders         list / stream orders
  trades         list / stream trades
  public-trades  stream the public trade tape
  public-book    stream resting-order state changes
  scenarios      list / start / next / prev / jump / stop
  weather        print the next live weather frame (--live keeps it open)
```

Global options:

```
  --addr ADDR           gRPC endpoint (default http://[::1]:8810; trading + weather)
  --ui-addr ADDR        HTTP endpoint for scenarios (default http://127.0.0.1:8811)
```

Examples:

```sh
# Place a buy on the first registered area + next 15-min boundary
tsctl place --gridpool 1 --side buy --price 85.00 --qty 1.0

# Stream live order updates for your test gridpool
tsctl orders --gridpool 1 --live

# Watch every public print on the exchange
tsctl public-trades --live

# Fire a scarcity scenario, see what your app does with sudden price spikes
tsctl scenarios start scarcity-spike

# What does the next weather frame look like?
tsctl weather
```

`--start` on `tsctl place` accepts `"next"`, `"+N"` (quarters from
the next 15-min boundary), or an RFC-3339 timestamp.

## The browser UI

Useful for eyeballing what your app is reacting to. The UI is a
[Leptos](https://leptos.dev/) SPA compiled to WebAssembly by
[`trunk`](https://trunkrs.dev/) (`cd web && trunk build`); the
resulting `web/dist/` is rust-embedded into the host binary so
once built it ships with the server. `cargo build` works before
`trunk build` has run — the SPA routes just 404 until the bundle
is populated. The UI shows:

- **Order book** ladders per area for one selected delivery period
- **Public trade tape** with a delivery-period filter
- **Weather panel** showing solar / wind / temperature per area
- **Scenarios panel** with timeline, stage list, and prev/next/stop
- **Chart panel** with a price tape per area
- **Pulse bar**: trade-count sparkbars, system pills, density + UTC/local toggles, live clock

Trade and book updates flow over WebSockets so the page is live
without polling.

## Constraints worth knowing about

- Only `LIMIT` orders are supported. Other types reject at admit
  time. `IOC` and `FOK` are honoured.
- Only `DELIVERY_DURATION_15` (15-minute contracts) are admitted.
- All delivery periods are in UTC at the wire.
- Internal physics + scenario stage hours run in the configured
  timezone (`Europe/Berlin` by default); `(set-timezone …)`
  redirects.
- The simulator is single-process and in-memory — no persistence
  across restarts.

## Development

### Building

```sh
cargo build              # host binary (debug)
cargo build --release    # host binary (optimised)
(cd web && trunk build)  # browser UI bundle into web/dist/
```

The proto submodule is pinned (`submodules/frequenz-api-electricity-trading`
at `3a41f88`, its nested `frequenz-api-common` at `fc70cb9`).
`build.rs` points `tonic-prost-build` at both. The env var
`TRADINGSIM_PROTO_ROOT` overrides the submodule path for downstream
packagers.

### Running tests

```sh
cargo test                            # everything
cargo test --lib                      # unit tests
cargo test --test grpc_e2e            # gRPC integration
cargo test --test ui_e2e              # UI HTTP integration
cargo test --test weather_e2e         # weather gRPC integration
```

The integration tests spawn the server on `127.0.0.1:0` and drive
it through generated clients — fast enough to run on every change.

### Logs

`simplelog` — `RUST_LOG=debug` for noisier output; `info` is the
default.

### Roadmap

[`todo.org`](todo.org) tracks pending work in org-mode style.
Items are marked `TODO` / `IN-PROGRESS` / `DONE`.

## License

GPL-3.0 — see [`LICENSE`](LICENSE).
