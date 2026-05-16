//! /api/scenarios polled list + Start / Prev / Next / Stop controls.

use std::time::Duration;

use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::types::Scenario;

const SCENARIOS_POLL: Duration = Duration::from_secs(2);

async fn fetch_scenarios() -> Option<Vec<Scenario>> {
    Request::get("/api/scenarios").send().await.ok()?.json().await.ok()
}

#[component]
pub fn Scenarios() -> impl IntoView {
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
                    move |_| act(n.clone(), "start")
                };
                let on_prev = {
                    let n = n.clone();
                    move |_| act(n.clone(), "prev")
                };
                let on_next = {
                    let n = n.clone();
                    move |_| act(n.clone(), "next")
                };
                let on_stop = {
                    let n = n.clone();
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
