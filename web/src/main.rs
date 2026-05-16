#![allow(non_snake_case)] // Leptos components are PascalCase by convention.

use std::time::Duration;

use futures::StreamExt;
use gloo_net::http::Request;
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

mod types;
use types::{ClockResp, InfoResp, PublicTrade, Scenario};

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
    let (latest, set_latest) = signal(None::<PublicTrade>);
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
                set_latest.set(Some(t));
            }
        }
    });

    let trades_line = move || match latest.get() {
        Some(t) => format!(
            "trades: {} (last #{} @ {})",
            trade_count.get(),
            t.id,
            t.price,
        ),
        None => format!("trades: {}", trade_count.get()),
    };

    view! {
        <header class="page-header">
            <h1>"tradingsim"</h1>
            <span class="page-meta muted">{info_line}</span>
            <span class="page-meta muted">{tz_line}</span>
            <span class="page-meta muted">{trades_line}</span>
        </header>
        <Scenarios/>
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
