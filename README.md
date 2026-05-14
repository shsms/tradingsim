# tradingsim

A simulator of a continuous intraday electricity exchange that speaks the
[Frequenz Electricity Trading gRPC API](https://github.com/frequenz-floss/frequenz-api-electricity-trading).
Reference behaviour is a continuous intraday electricity exchange
for the DE-LU bidding zone, but every market knob is data-driven through
a tulisp configuration file so the same binary can stand in for FR / AT /
NL / BE or a synthetic zone.

Comes with synthetic market-makers, configurable aggressor flow, a
weather forecast service (sibling gRPC API), time-of-day scenarios, a
browser UI, and a `tsctl` companion CLI.

## Table of contents

- [Quick start](#quick-start)
- [What's inside](#whats-inside)
- [Configuration](#configuration)
- [Scenarios](#scenarios)
- [The `tsctl` CLI](#the-tsctl-cli)
- [The browser UI](#the-browser-ui)
- [Programmatic clients (gRPC)](#programmatic-clients-grpc)
- [Development](#development)

## Quick start

Prerequisites: Rust stable (edition 2024), `git`, and a checkout with the
submodules initialised — the proto definitions live in
`submodules/frequenz-api-electricity-trading`.

```sh
git clone <repo> tradingsim
cd tradingsim
git submodule update --init --recursive
cargo build --release
```

Run the server:

```sh
./target/release/tradingsim
```

By default the server loads `config.lisp` from the current directory and
binds:

| Service                  | Address              |
| ------------------------ | -------------------- |
| Electricity Trading gRPC | `[::1]:8810`         |
| UI server (HTTP)         | `127.0.0.1:8811`     |
| Weather Forecast gRPC    | `[::1]:8812`         |

Point a browser at <http://127.0.0.1:8811/> to see the live order book,
trade tape, weather panel, and scenario controls.

In another shell, place an order via `tsctl`:

```sh
./target/release/tsctl place --gridpool 1 --side buy --price 85.00 --qty 1.0
./target/release/tsctl orders --gridpool 1
```

## What's inside

```
src/
  bin/
    tradingsim.rs       # gRPC + UI server entry point
    tsctl.rs            # CLI client (delegates to gRPC + UI HTTP)
  lib.rs                # module roots
  proto.rs              # tonic include of the generated proto
  proto_conv.rs         # bidirectional sim ↔ proto bridges
  server.rs             # ElectricityTradingServer (11 RPCs)
  weather_server.rs     # WeatherForecastServer (live + history)
  scenarios.rs          # scenario registry + bias-tick task
  ui/mod.rs             # axum-driven UI server + JSON endpoints
  lisp/                 # tulisp config DSL: defuns + Config struct
  sim/
    clock.rs            # configurable Tz (Europe/Berlin default)
    book.rs             # half-book + price-keyed FIFO levels
    counterparty.rs     # MarketMaker + Aggressor engines
    curve.rs            # forward price curve
    decimal.rs          # snap-to-tick + step helpers
    gridpool.rs         # per-portfolio order/trade index
    market.rs           # Area, Currency, MarketRules, DeliveryPeriod
    matching.rs         # continuous matcher (LIMIT, FOK, IOC)
    order.rs            # Order + state machines
    trade.rs            # Trade + PublicTrade
    weather.rs          # WeatherRegistry, solar / wind / temp models
    world.rs            # owns markets, gridpools, books, broadcasters
ui-assets/
  index.html            # embedded SPA (HTML + CSS + JS in one file)
scenarios/              # canned time-of-day scenarios (lisp)
config.lisp             # sample top-level configuration
sim/common.lisp         # fleet helpers, every / run-with-timer sugar
tests/                  # integration tests against the live services
plan.org                # canonical roadmap + design notes
```

Two binaries — `tradingsim` (server) and `tsctl` (CLI). Both built from
the same crate.

## Configuration

Configuration is a [tulisp](https://github.com/shsms/tulisp) file. The
binary evaluates it at startup. Hot reload is on: save the file and the
running server picks up the new values on the next bias tick or scenario
fire (your timers keep running across reloads).

A minimal config:

```lisp
(unless (boundp 'tradingsim-loaded)
  (setq tradingsim-loaded t)
  (load "sim/common.lisp"))

(reset-state)                          ;; clear timers from previous load

(set-socket-addr "[::1]:8810")          ;; gRPC bind
(set-physics-tick-ms 100)               ;; matcher loop cadence
(set-timezone "Europe/Berlin")          ;; physics + scenario tz

(register-markets '("10Y1001A1001A82H"))                   ;; DE-LU
(%make-gridpool :id 1 :name "default"
                :areas '("10Y1001A1001A82H"))

(mm-fleet :area "10Y1001A1001A82H" :prefix "de")           ;; 48 MMs
(aggressor-fleet :area "10Y1001A1001A82H" :prefix "de")    ;; flow
```

See `config.lisp` for a populated example (four German TSO zones, four
neighbouring countries, weather locations, scenarios).

### Available defuns

**Transport + clock**

| Defun                                  | Effect                                                  |
| -------------------------------------- | ------------------------------------------------------- |
| `(set-socket-addr "[::1]:8810")`       | gRPC bind address                                       |
| `(set-physics-tick-ms 100)`            | matcher loop cadence (ms)                               |
| `(set-timezone "Europe/Berlin")`       | timezone the physics + scenario hours run in           |
| `(set-weather-stream-cadence-seconds N)` | how often `ReceiveLiveWeatherForecast` emits          |

**Markets, gridpools, couplings**

| Defun                                              | Effect                              |
| -------------------------------------------------- | ----------------------------------- |
| `(%make-market :area EIC :currency "eur")`         | register a market                   |
| `(register-markets '(EIC ...))`                    | sugar: register many at once       |
| `(%make-gridpool :id N :name S :areas '(...))`     | register a portfolio               |
| `(%make-coupling :areas '(A B) :gate-offset-seconds N)` | couple two areas                |
| `(couple-all-pairs '(...))`                        | K_n graph couple                    |
| `(couple-pairs-across '(...) '(...))`              | bipartite couple                    |

**Market-makers + aggressors**

| Defun                                                       | Effect                                  |
| ----------------------------------------------------------- | --------------------------------------- |
| `(%make-market-maker :name S :area EIC :quarter-offset N ...)` | one MM quoting one quarter           |
| `(mm-fleet :area EIC :prefix S)`                            | 48 MMs covering the next 12 hours      |
| `(%make-aggressor :name S :area EIC :rate-ms N :size MW ...)` | one taker firing on a schedule       |
| `(aggressor-fleet :area EIC :prefix S)`                     | 4 profiles × 48 quarters                |
| `(set-mm-bias-scale 25.0)`                                  | EUR per (bias - 0.5) tilt unit          |
| `(set-mm-reference NAME EUR)`                               | snap an MM's reference price            |
| `(set-mm-{spread,size,demand,surplus,noise} NAME VAL)`      | tune one knob on one MM                 |
| `(set-mm-follow-last-trade NAME RATE)`                      | 0 = static, 1 = snap to last trade       |
| `(set-aggressor-{size,side-bias} NAME VAL)`                 | tune one knob on one aggressor          |

**Weather**

| Defun                                                                          | Effect                              |
| ------------------------------------------------------------------------------ | ----------------------------------- |
| `(%make-weather-location :name S :area EIC :lat F :lon F :cloud-cover F :mean-wind F)` | register an atmospheric anchor      |
| `(set-weather-cloud-cover F)`                                                  | mutate the default location         |
| `(set-weather-{mean-wind,direction,temperature-base} VAL)`                     | mutate the default location         |

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

**Scenarios** — see [the next section](#scenarios).

**Timers + reload**

| Defun                                       | Effect                                                          |
| ------------------------------------------- | --------------------------------------------------------------- |
| `(every :milliseconds N :call (lambda () ...))` | periodic callback (sugar over `run-with-timer`)              |
| `(run-with-timer FIRST REPEAT FN)`          | tulisp-async primitive                                          |
| `(reset-state)`                             | cancel all timers; call at the top of `config.lisp` for hot reload |
| `(watch-file PATH)`                         | also reload when this support file changes                      |

## Scenarios

A *scenario* is a named, staged perturbation of the natural duck curve.
Each stage carries an hour window, a bias range (0.0 = sell-heavy, 1.0 =
buy-heavy), and optional weather overrides (cloud cover / wind /
temperature). The bias-tick task interpolates between stages as the
wallclock advances and feeds the result to the market-makers and
aggressors.

Defining one in lisp:

```lisp
(define-scenario "morning-ramp"
  :description "Cold winter morning waking up"
  :date "2026-01-15"
  :stages '(("00:00-05:00 quiet"       0.50 0.50)
            ("05:00-08:00 ramp"        0.55 0.75)
            ("08:00-10:00 peak"        0.75 0.65)
            ("10:00-24:00 normal day"  0.50 0.50)))
```

Several canned scenarios ship in `scenarios/` (sunny summer day, rainy
summer day, sunny summer holiday, winter weekday, windy winter night,
scarcity spike, buy-only flow). `config.lisp` loads them at startup.

Run one from the UI's Scenarios panel, from `tsctl`, or programmatically:

```sh
./target/release/tsctl scenarios list
./target/release/tsctl scenarios start sunny-summer-day
./target/release/tsctl scenarios next sunny-summer-day
./target/release/tsctl scenarios stop sunny-summer-day
```

Stage hour windows are in the *configured timezone* (`Europe/Berlin` by
default), so a stage like `"13:00-16:00 belly"` means 13:00 local. The UI
header has a chip that toggles display between local and UTC; the
underlying scenario hours are always local.

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
  weather        print the next live weather frame (--live to keep going)
```

Global options:

```
  --addr ADDR           gRPC endpoint (default http://[::1]:8810)
  --ui-addr ADDR        HTTP endpoint for scenarios (default http://127.0.0.1:8811)
  --weather-addr ADDR   gRPC endpoint for the weather service (default http://[::1]:8812)
```

Examples:

```sh
# Place a buy
tsctl place --gridpool 1 --side buy --price 85.00 --qty 1.0

# Stream live orders for a gridpool
tsctl orders --gridpool 1 --live

# Watch the public trade tape
tsctl public-trades --live

# Start a scenario
tsctl scenarios start sunny-summer-day

# Get a weather snapshot
tsctl weather
```

`--start` on `tsctl place` accepts `"next"`, `"+N"` (quarters from the
next boundary), or an RFC-3339 timestamp.

## The browser UI

The UI server is an axum SPA embedded into the binary (no separate build
step). It serves:

- **Order book** ladders per area for one selected delivery period
- **Public trade tape** with a filter dropdown
- **Weather panel** showing solar / wind / temperature per area
- **Scenarios panel** with timeline, stage list, and prev/next/stop controls
- **Chart panel** with a price tape per area
- **Pulse bar** at the top: trade count sparkbars per area, system pills,
  density toggle, UTC/local toggle, and the live clock

The UI talks to:

- HTTP `GET /api/info`, `/api/clock`, `/api/gridpools`, `/api/scenarios`,
  `/api/weather`
- HTTP `POST /api/scenarios/{name}/{start|next|prev|jump/{i}|stop}`
- WebSocket `/ws/public-trades` and `/ws/public-book`

All trade and book updates flow over WebSockets so the page is live
without polling.

## Programmatic clients (gRPC)

The Electricity Trading service implements all 11 RPCs from the
Frequenz proto. The official Python client is
[frequenz-client-electricity-trading-python](https://github.com/frequenz-floss/frequenz-client-electricity-trading-python).
Point it at `[::1]:8810` and exercise:

- `CreateGridpoolOrder` / `GetGridpoolOrder` / `UpdateGridpoolOrder`
- `CancelGridpoolOrder` / `CancelAllGridpoolOrders`
- `ListGridpoolOrders` / `ListGridpoolTrades`
- `ReceiveGridpoolOrdersStream` / `ReceiveGridpoolTradesStream`
- `ReceivePublicTradesStream` / `ReceivePublicOrderBookStream` *(planned)*

The weather service implements
`ReceiveLiveWeatherForecast` + `ReceiveHistoricalWeatherForecast`, with
each frame carrying 24 hourly forecasts for every registered location
(solar, 100 m u/v wind components, 2 m temperature). The forecast cadence
defaults to 1 hour and is configurable via
`(set-weather-stream-cadence-seconds N)`.

Constraints worth knowing about:

- Only `LIMIT` orders are supported. Other order types are nice-to-have.
- Only `DELIVERY_DURATION_15` (15-minute contracts) are supported.
- All delivery periods are in UTC at the wire.
- The internal physics + scenario stage hours run in the configured
  timezone (Europe/Berlin by default); `set-timezone` lets you redirect.

## Development

### Building

```sh
cargo build              # debug
cargo build --release    # optimised
```

The proto submodule is pinned (`submodules/frequenz-api-electricity-trading`
at `3a41f88`, its nested `frequenz-api-common` at `fc70cb9`). `build.rs`
points `tonic-prost-build` at both. The env var `TRADINGSIM_PROTO_ROOT`
overrides the submodule path for downstream packagers.

### Running tests

```sh
cargo test                            # everything
cargo test --lib                      # unit tests only
cargo test --test grpc_e2e            # gRPC integration
cargo test --test ui_e2e              # UI HTTP integration
cargo test --test weather_e2e         # weather gRPC integration
```

The integration tests in `tests/` spawn the server on `127.0.0.1:0`
(ephemeral port) and drive it through the generated client — fast enough
to run on every change.

### Logs

The binary uses `simplelog`. Set `RUST_LOG=debug` for noisier output;
`info` is the default.

### Adding a sim type

1. Pure data + small helpers, no I/O, no async.
2. Inline `#[cfg(test)] mod tests` covering the invariants downstream
   code relies on.
3. Re-export from `src/sim/mod.rs`.
4. Proto bridging lives in `src/proto_conv.rs` — not in the type's own
   module.

### Roadmap

`plan.org` is authoritative for scope, market model, lisp DSL design,
the 11-RPC gRPC mapping, UI shape, scenarios, testing strategy, and the
phase-by-phase roadmap. Use it to figure out what to build; use this
README to figure out how to run it.

## License

GPL-3.0 — see [`LICENSE`](LICENSE).
