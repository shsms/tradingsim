#![allow(non_snake_case)] // Leptos components are PascalCase by convention.

use std::time::Duration;

use futures::StreamExt;
use gloo_net::http::Request;
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

mod types;
use types::{ClockResp, InfoResp, PublicTrade, Scenario, WeatherLoc};

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| view! { <Shell/> });
}

#[component]
fn Shell() -> impl IntoView {
    // Initial /api/info + /api/clock fetch. Both are fired once on
    // mount; refresh-on-tick comes when the header pulse bar lands.
    let info = LocalResource::new(|| async {
        Request::get("/api/info").send().await.ok()?.json::<InfoResp>().await.ok()
    });
    let clock = LocalResource::new(|| async {
        Request::get("/api/clock").send().await.ok()?.json::<ClockResp>().await.ok()
    });

    let info_line = move || match info.get().as_deref() {
        Some(Some(i)) => format!(
            "v{} · {} gridpool{} · {} markets · {} couplings",
            i.version,
            i.gridpools,
            if i.gridpools == 1 { "" } else { "s" },
            i.markets,
            i.couplings,
        ),
        Some(None) => "—".to_string(),
        None => "loading…".to_string(),
    };

    let tz_line = move || match clock.get().as_deref() {
        Some(Some(c)) => format!("tz: {}", c.tz),
        _ => String::new(),
    };

    // Live trade tape counter. /ws/public-trades emits a snapshot
    // (the recent history ring) on connect, then one frame per
    // print. Counting them is enough to prove the stream end-to-end
    // — the proper tape panel lands next.
    let (trade_count, set_trade_count) = signal(0_usize);
    let (trades, set_trades) = signal(Vec::<PublicTrade>::new());
    // The recent-trades ring is consumed by the public-trades panel
    // (and later the price chart + sparkbars). Provide it via context
    // so the WS plumbing stays in one place.
    provide_context(trades);
    leptos::task::spawn_local(async move {
        let mut ws = match WebSocket::open("/ws/public-trades") {
            Ok(ws) => ws,
            Err(e) => {
                leptos::logging::log!("ws open failed: {e:?}");
                return;
            }
        };
        while let Some(msg) = ws.next().await {
            if let Ok(Message::Text(s)) = msg
                && let Ok(t) = serde_json::from_str::<PublicTrade>(&s)
            {
                set_trade_count.update(|n| *n += 1);
                set_trades.update(|v| {
                    v.insert(0, t);
                    if v.len() > TRADES_BUFFER_CAP {
                        v.truncate(TRADES_BUFFER_CAP);
                    }
                });
            }
        }
    });

    let trades_line = move || {
        let latest = trades.with(|v| v.first().cloned());
        match latest {
            Some(t) => format!(
                "trades: {} (last #{} @ {})",
                trade_count.get(),
                t.id,
                t.price,
            ),
            None => format!("trades: {}", trade_count.get()),
        }
    };

    view! {
        <header class="page-header">
            <h1>"tradingsim"</h1>
            <span class="page-meta muted">{info_line}</span>
            <span class="page-meta muted">{tz_line}</span>
            <span class="page-meta muted">{trades_line}</span>
        </header>
        <Scenarios/>
        <Weather/>
        <PublicTrades/>
    }
}

// Matches the JS UI's TRADES_BUFFER_CAP — enough history to back
// the price chart's resampler without growing unbounded.
const TRADES_BUFFER_CAP: usize = 500;
const TRADES_DISPLAY_CAP: usize = 10;

const WEATHER_POLL: Duration = Duration::from_secs(10);

async fn fetch_weather() -> Option<Vec<WeatherLoc>> {
    Request::get("/api/weather").send().await.ok()?.json().await.ok()
}

#[component]
fn Weather() -> impl IntoView {
    let (locs, set_locs) = signal(Vec::<WeatherLoc>::new());
    let (loaded, set_loaded) = signal(false);

    leptos::task::spawn_local(async move {
        loop {
            if let Some(list) = fetch_weather().await {
                set_locs.set(list);
                set_loaded.set(true);
            }
            TimeoutFuture::new(WEATHER_POLL.as_millis() as u32).await;
        }
    });

    let body = move || {
        let list: Vec<_> = locs
            .get()
            .into_iter()
            // Hide the unlinked fallback location; every configured
            // area gets its own slot in the shipping config.
            .filter(|l| l.area_code.is_some())
            .collect();
        if list.is_empty() {
            return view! {
                <i class="muted">
                    {move || if loaded.get() { "no weather locations" } else { "loading…" }}
                </i>
            }
            .into_any();
        }
        view! {
            <div class="weather-grid">
                {list.into_iter().map(|l| view! { <WeatherCell loc=l/> }).collect_view()}
            </div>
        }
        .into_any()
    };

    view! {
        <section class="panel panel-weather">
            <h2>"Weather (now)"</h2>
            {body}
        </section>
    }
}

#[component]
fn WeatherCell(loc: WeatherLoc) -> impl IntoView {
    let (open, set_open) = signal(false);
    let tag = loc
        .area_code
        .as_deref()
        .map(area_tag)
        .unwrap_or("—")
        .to_string();
    view! {
        <div
            class="weather-cell"
            class:open=move || open.get()
            on:click=move |_| set_open.update(|o| *o = !*o)
        >
            <div class="weather-head">
                <span class="area-badge">{tag}</span>
                <span class="muted">{format!("☁ {:.2}", loc.cloud_cover)}</span>
            </div>
            <div class="weather-metric">
                "solar " <span class="muted">{format!("{} W/m²", loc.solar_now.round() as i64)}</span>
            </div>
            <div class="weather-metric">
                "wind " <span class="muted">{format!("{:.1} m/s", loc.wind_now)}</span>
            </div>
            <div class="weather-metric">
                "temp " <span class="muted">{format!("{:.1} °C", loc.temp_c_now)}</span>
            </div>
            <div class="weather-detail">
                {format!("lat {:.1} · lon {:.1}", loc.lat, loc.lon)}<br/>
                {format!("wind direction {}°", loc.wind_direction.round() as i64)}<br/>
                {format!("mean wind {:.1} m/s", loc.mean_wind)}
            </div>
        </div>
    }
}

#[component]
fn PublicTrades() -> impl IntoView {
    let trades = expect_context::<ReadSignal<Vec<PublicTrade>>>();

    let body = move || {
        let rows: Vec<_> = trades.with(|v| v.iter().take(TRADES_DISPLAY_CAP).cloned().collect());
        if rows.is_empty() {
            return view! { <tr><td colspan="6" class="muted"><i>"awaiting prints…"</i></td></tr> }
                .into_any();
        }
        rows.into_iter()
            .map(|t| {
                let area_cell = if t.buy_area == t.sell_area {
                    view! {
                        <td><span class="area-badge">{area_tag(&t.buy_area)}</span></td>
                    }
                    .into_any()
                } else {
                    view! {
                        <td>
                            <span class="area-badge">{area_tag(&t.buy_area)}</span>
                            <span class="area-cross">"→"</span>
                            <span class="area-badge">{area_tag(&t.sell_area)}</span>
                        </td>
                    }
                    .into_any()
                };
                view! {
                    <tr>
                        <td>{format!("#{}", t.id)}</td>
                        <td>{t.quantity}</td>
                        <td>{t.price}</td>
                        {area_cell}
                        <td class="muted">{short_time_utc(&t.period)}</td>
                        <td class="muted">{short_time_sec_utc(&t.execution_time)}</td>
                    </tr>
                }
                .into_any()
            })
            .collect_view()
            .into_any()
    };

    view! {
        <section class="panel panel-trades">
            <div class="book-head">
                <h2>"Public trades"</h2>
            </div>
            <div class="scroll">
                <table class="trades-table">
                    <colgroup>
                        <col class="col-id"/>
                        <col class="col-qty"/>
                        <col class="col-price"/>
                        <col class="col-area"/>
                        <col class="col-delivery"/>
                        <col class="col-exec"/>
                    </colgroup>
                    <thead><tr>
                        <th>"id"</th><th>"qty"</th><th>"price"</th>
                        <th>"area"</th><th>"delivery"</th><th>"exec"</th>
                    </tr></thead>
                    <tbody>{body}</tbody>
                </table>
            </div>
        </section>
    }
}

/// Slice HH:MM out of an RFC-3339 UTC timestamp. The JS UI uses
/// `Intl.DateTimeFormat` keyed on the configured sim tz; that
/// arrives with the pulse-bar commit. Until then, prints carry
/// the wire-side UTC time.
fn short_time_utc(iso: &str) -> String {
    iso.get(11..16).unwrap_or("--:--").to_string()
}

fn short_time_sec_utc(iso: &str) -> String {
    iso.get(11..19).unwrap_or("--:--:--").to_string()
}

/// Short tag for an EIC area code — `10YDE-EON------1` → `TN`, etc.
/// Mirrors the JS UI's ALL_AREAS table.
fn area_tag(code: &str) -> &'static str {
    match code {
        "10YDE-EON------1" => "TN",
        "10YDE-RWENET---I" => "AM",
        "10YDE-VE-------2" => "HZ",
        "10YDE-ENBW-----N" => "BW",
        "10YFR-RTE------C" => "FR",
        "10YNL----------L" => "NL",
        "10YBE----------2" => "BE",
        "10YAT-APG------L" => "AT",
        _ => "?",
    }
}

const SCENARIOS_POLL: Duration = Duration::from_secs(2);

async fn fetch_scenarios() -> Option<Vec<Scenario>> {
    Request::get("/api/scenarios").send().await.ok()?.json().await.ok()
}

#[component]
fn Scenarios() -> impl IntoView {
    let (scenarios, set_scenarios) = signal(Vec::<Scenario>::new());
    let (loaded, set_loaded) = signal(false);

    // 2s poll matching the JS UI cadence. Action handlers fire an
    // immediate refetch so the panel updates without waiting for
    // the next tick.
    leptos::task::spawn_local(async move {
        loop {
            if let Some(list) = fetch_scenarios().await {
                set_scenarios.set(list);
                set_loaded.set(true);
            }
            TimeoutFuture::new(SCENARIOS_POLL.as_millis() as u32).await;
        }
    });

    let act = move |name: String, action: &'static str| {
        // Scenario names come from the lisp config — slug-style
        // identifiers in practice, so no URL encoding needed here
        // even though the JS UI defensively encodes them.
        leptos::task::spawn_local(async move {
            let url = format!("/api/scenarios/{name}/{action}");
            let _ = Request::post(&url).send().await;
            if let Some(list) = fetch_scenarios().await {
                set_scenarios.set(list);
            }
        });
    };

    let body = move || {
        let list = scenarios.get();
        if list.is_empty() {
            return view! {
                <i class="muted">
                    {move || if loaded.get() { "no scenarios registered" } else { "loading…" }}
                </i>
            }
            .into_any();
        }
        let rows = list
            .into_iter()
            .map(|s| {
                let n = s.name.clone();
                let summary = match s.current_stage {
                    Some(idx) => format!(
                        "{} · stage {}/{}",
                        s.name,
                        idx + 1,
                        s.stages.len()
                    ),
                    None => s.name.clone(),
                };
                let active = s.current_stage.is_some();
                let on_start = {
                    let n = n.clone();
                    let act = act.clone();
                    move |_| act(n.clone(), "start")
                };
                let on_prev = {
                    let n = n.clone();
                    let act = act.clone();
                    move |_| act(n.clone(), "prev")
                };
                let on_next = {
                    let n = n.clone();
                    let act = act.clone();
                    move |_| act(n.clone(), "next")
                };
                let on_stop = {
                    let n = n.clone();
                    let act = act.clone();
                    move |_| act(n.clone(), "stop")
                };
                view! {
                    <div class="scenario">
                        <div class="scenario-head">
                            <strong>{summary}</strong>
                            <span class="muted">{s.description}</span>
                            <span class="scenario-controls" style="margin-left:auto">
                                {(!active).then(|| view! {
                                    <button on:click=on_start>"Start"</button>
                                })}
                                {active.then(|| view! {
                                    <>
                                        <button on:click=on_prev>"Prev"</button>
                                        <button on:click=on_next>"Next"</button>
                                        <button on:click=on_stop>"Stop"</button>
                                    </>
                                })}
                            </span>
                        </div>
                    </div>
                }
            })
            .collect_view();
        view! { <div>{rows}</div> }.into_any()
    };

    view! {
        <section class="panel panel-scenarios">
            <h2>"Scenarios"</h2>
            {body}
        </section>
    }
}
