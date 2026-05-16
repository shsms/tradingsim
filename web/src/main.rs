#![allow(non_snake_case)] // Leptos components are PascalCase by convention.

use futures::StreamExt;
use gloo_net::http::Request;
use gloo_net::websocket::{Message, futures::WebSocket};
use leptos::prelude::*;

mod panels;
mod types;
mod util;

use panels::{PublicTrades, Scenarios, TRADES_BUFFER_CAP, Weather};
use types::{ClockResp, InfoResp, PublicTrade};

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

    // WS-driven trade ring shared with the public-trades panel (and
    // eventually the price chart + sparkbars) via context.
    let (trade_count, set_trade_count) = signal(0_usize);
    let (trades, set_trades) = signal(Vec::<PublicTrade>::new());
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
