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
    ContractPill, FilterBar, Gridpools, PriceChart, PublicTrades, PulseBar, Scenarios, SparkState,
    TzMode, TRADES_BUFFER_CAP, Weather, load_filter, load_tz_mode,
};
use types::{ClockResp, InfoResp, PublicTrade, Scenario};

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| view! { <Shell/> });
}

#[component]
fn Shell() -> impl IntoView {
    // Filter state shared with FilterBar, Weather, PublicTrades —
    // hydrated from localStorage so chip picks survive reload.
    provide_context(RwSignal::new(load_filter()));

    // Sim tz + display tz lifted to context. sim_tz tracks the
    // simulator's home zone (from /api/clock); tz_mode is the user's
    // chip preference (Local | Utc); display_tz is the effective
    // formatter input — sim_tz when Local, "UTC" otherwise — and is
    // what every panel reads to format prints.
    let sim_tz = RwSignal::new(String::from("UTC"));
    let tz_mode = RwSignal::new(load_tz_mode());
    let display_tz = RwSignal::new(String::from("UTC"));
    Effect::new(move |_| {
        let effective = match tz_mode.get() {
            TzMode::Local => sim_tz.get(),
            TzMode::Utc => "UTC".to_string(),
        };
        display_tz.set(effective);
    });
    // Only display_tz + tz_mode are exposed — panels format with the
    // first, the pulse-bar chip mutates the second. sim_tz stays
    // Shell-scoped behind the Effect that drives display_tz.
    provide_context(tz_mode);
    provide_context(display_tz);
    leptos::task::spawn_local(async move {
        if let Ok(r) = Request::get("/api/clock").send().await
            && let Ok(c) = r.json::<ClockResp>().await
        {
            sim_tz.set(c.tz);
        }
    });

    // Scenarios polling lives in Shell so the pulse bar's scenario
    // indicator and the Scenarios panel share one fetch loop.
    let scenarios = RwSignal::new(Vec::<Scenario>::new());
    provide_context(scenarios);
    leptos::task::spawn_local(async move {
        loop {
            if let Ok(r) = Request::get("/api/scenarios").send().await
                && let Ok(list) = r.json::<Vec<Scenario>>().await
            {
                scenarios.set(list);
            }
            gloo_timers::future::TimeoutFuture::new(2000).await;
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

    // Click-pinned delivery period: a trade row sets this, the chart
    // honours it as a soft pin (overrides the auto-pick but yields to
    // the chart's own dropdown), the trades panel scopes to it, and
    // the filter bar surfaces a clearable pill while it's set.
    let focused_period = RwSignal::new(None::<String>);
    provide_context(focused_period);

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

    view! {
        <header class="page-header">
            <h1>"tradingsim"</h1>
            <span class="page-meta muted">{info_line}</span>
        </header>
        <PulseBar/>
        <div class="grid">
            <div class="tier-row tier-chart-scenarios">
                <PriceChart/>
                <Scenarios/>
            </div>
            <Gridpools/>
            <ContractPill/>
            <FilterBar/>
            <div class="tier-row tier-weather-trades">
                <Weather/>
                <PublicTrades/>
            </div>
        </div>
    }
}
