//! Scenarios panel — reads the shared `RwSignal<Vec<Scenario>>` from
//! context (Shell owns the polling) and offers idle-picker / active
//! controls. Start / Prev / Next / Stop fire the matching POST then
//! immediately refresh the shared signal.

use gloo_net::http::Request;
use leptos::prelude::*;

use crate::types::Scenario;

async fn fetch_scenarios() -> Option<Vec<Scenario>> {
    Request::get("/api/scenarios").send().await.ok()?.json().await.ok()
}

async fn post_action(name: &str, action: &str) {
    let url = format!("/api/scenarios/{name}/{action}");
    let _ = Request::post(&url).send().await;
}

#[component]
pub fn Scenarios() -> impl IntoView {
    let scenarios = expect_context::<RwSignal<Vec<Scenario>>>();

    let act = move |name: String, action: &'static str| {
        leptos::task::spawn_local(async move {
            post_action(&name, action).await;
            if let Some(list) = fetch_scenarios().await {
                scenarios.set(list);
            }
        });
    };

    let switch_to = move |target: String| {
        leptos::task::spawn_local(async move {
            let list = fetch_scenarios().await.unwrap_or_default();
            if let Some(active) = list.iter().find(|s| s.current_stage.is_some()) {
                post_action(&active.name, "stop").await;
            }
            post_action(&target, "start").await;
            if let Some(list) = fetch_scenarios().await {
                scenarios.set(list);
            }
        });
    };

    let body = move || {
        let list = scenarios.get();
        if list.is_empty() {
            return view! { <i class="muted">"no scenarios registered"</i> }.into_any();
        }
        let active = list.iter().find(|s| s.current_stage.is_some()).cloned();
        match active {
            Some(a) => {
                let inactive: Vec<_> = list
                    .into_iter()
                    .filter(|s| s.current_stage.is_none())
                    .collect();
                view! { <ActiveScenarioRow scenario=a others=inactive on_action=act on_switch=switch_to/> }
                    .into_any()
            }
            None => view! { <IdlePicker scenarios=list on_start=act/> }.into_any(),
        }
    };

    view! {
        <section class="panel panel-scenarios">
            <h2>"Scenarios"</h2>
            {body}
        </section>
    }
}

#[component]
fn IdlePicker<F>(scenarios: Vec<Scenario>, on_start: F) -> impl IntoView
where
    F: Fn(String, &'static str) + Copy + 'static,
{
    let on_change = move |ev| {
        let name = event_target_value(&ev);
        if !name.is_empty() {
            on_start(name, "start");
        }
    };
    view! {
        <div class="scenario-picker">
            <span class="muted">"Start a scenario:"</span>
            <select on:change=on_change>
                <option value="">"— pick one —"</option>
                {scenarios.into_iter().map(|s| view! {
                    <option value=s.name.clone()>
                        {format!("{} — {}", s.name, s.description)}
                    </option>
                }).collect_view()}
            </select>
        </div>
    }
}

#[component]
fn ActiveScenarioRow<F, S>(
    scenario: Scenario,
    others: Vec<Scenario>,
    on_action: F,
    on_switch: S,
) -> impl IntoView
where
    F: Fn(String, &'static str) + Copy + 'static,
    S: Fn(String) + Copy + 'static,
{
    let cur = scenario.current_stage.unwrap_or(0);
    let last = scenario.stages.len().saturating_sub(1);
    let n = scenario.name.clone();
    let summary = format!(
        "{} · stage {}/{}",
        scenario.name,
        cur + 1,
        scenario.stages.len()
    );
    let prev_dis = cur == 0;
    let next_dis = cur >= last;

    let on_prev = {
        let n = n.clone();
        move |_| on_action(n.clone(), "prev")
    };
    let on_next = {
        let n = n.clone();
        move |_| on_action(n.clone(), "next")
    };
    let on_stop = {
        let n = n.clone();
        move |_| on_action(n.clone(), "stop")
    };
    let on_switch_change = move |ev| {
        let v = event_target_value(&ev);
        if !v.is_empty() {
            on_switch(v);
        }
    };

    view! {
        <div class="scenario active">
            <div class="scenario-head">
                <strong>{summary}</strong>
                {scenario.manual_override.then(|| view! {
                    <span class="badge-manual">"manual"</span>
                })}
                <span class="scenario-controls" style="margin-left:auto">
                    <button on:click=on_prev disabled=prev_dis>"Prev"</button>
                    <button on:click=on_next disabled=next_dis>"Next"</button>
                    <button on:click=on_stop>"Stop"</button>
                </span>
            </div>
            {(!others.is_empty()).then(|| view! {
                <div class="scenario-picker">
                    <span class="muted">"Switch to:"</span>
                    <select on:change=on_switch_change>
                        <option value="">"—"</option>
                        {others.into_iter().map(|s| {
                            view! { <option value=s.name.clone()>{s.name.clone()}</option> }
                        }).collect_view()}
                    </select>
                </div>
            })}
        </div>
    }
}
