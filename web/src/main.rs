#![allow(non_snake_case)] // Leptos components are PascalCase by convention.

use futures::StreamExt;
use gloo_net::http::Request;
use gloo_net::websocket::{Message, futures::WebSocket};
use leptos::prelude::*;

mod intl;
mod panels;
mod types;
mod util;

use panels::{
    FilterBar, Gridpools, PriceChart, PublicTrades, PulseBar, Scenarios, SparkState,
    TRADES_BUFFER_CAP, Weather, load_filter,
};
use types::{ClockResp, InfoResp, PublicTrade};

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| view! { <Shell/> });
}

#[component]
fn Shell() -> impl IntoView {
    // Filter state shared with FilterBar, Weather, PublicTrades —
    // hydrated from localStorage so chip picks survive reload.
    provide_context(RwSignal::new(load_filter()));

    // Sim tz lifted to context so every panel formats prints in the
    // simulator's home zone (Europe/Berlin in the shipped config)
    // rather than the browser's local zone. Defaults to "UTC" until
    // /api/clock returns, then panels re-render on the next signal
    // read.
    let sim_tz = RwSignal::new(String::from("UTC"));
    provide_context(sim_tz);
    leptos::task::spawn_local(async move {
        if let Ok(r) = Request::get("/api/clock").send().await
            && let Ok(c) = r.json::<ClockResp>().await
        {
            sim_tz.set(c.tz);
        }
    });

    let info = LocalResource::new(|| async {
        Request::get("/api/info").send().await.ok()?.json::<InfoResp>().await.ok()
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

    let tz_line = move || {
        let tz = sim_tz.get();
        if tz == "UTC" { String::new() } else { format!("tz: {tz}") }
    };

    // WS-driven shared state. Trades + trade_count feed the public
    // trades panel + the pulse-bar pills; spark_state feeds the
    // sparkbars; weather_loaded comes from the Weather panel's
    // first successful poll so the pulse-bar pill can light up.
    let (trade_count, set_trade_count) = signal(0_usize);
    let (trades, set_trades) = signal(Vec::<PublicTrade>::new());
    let spark = RwSignal::new(SparkState::default());
    let weather_loaded = RwSignal::new(false);
    provide_context(trades);
    provide_context(trade_count);
    provide_context(spark);
    provide_context(weather_loaded);
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
                let buy_area = t.buy_area.clone();
                set_trades.update(|v| {
                    v.insert(0, t);
                    if v.len() > TRADES_BUFFER_CAP {
                        v.truncate(TRADES_BUFFER_CAP);
                    }
                });
                spark.update(|s| s.record(&buy_area));
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
        <PulseBar/>
        <div class="grid">
            <div class="tier-row tier-chart-scenarios">
                <PriceChart/>
                <Scenarios/>
            </div>
            <Gridpools/>
            <FilterBar/>
            <div class="tier-row tier-weather-trades">
                <Weather/>
                <PublicTrades/>
            </div>
        </div>
    }
}
