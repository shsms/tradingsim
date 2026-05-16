//! Gridpool drill-down (3-pane master-detail): pool list, orders
//! for the selected pool, and trades for the selected order. All
//! three are driven by signals scoped to the Gridpools component;
//! later panes get wired in as the port lands them.

use std::time::Duration;

use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::types::GridpoolResp;
use crate::util::area_tag;

const GRIDPOOL_POLL: Duration = Duration::from_secs(3);

async fn fetch_pools() -> Option<Vec<GridpoolResp>> {
    Request::get("/api/gridpools").send().await.ok()?.json().await.ok()
}

#[component]
pub fn Gridpools() -> impl IntoView {
    let (pools, set_pools) = signal(Vec::<GridpoolResp>::new());
    let (selected, set_selected) = signal(None::<u64>);
    let (loaded, set_loaded) = signal(false);

    // 3 s poll matching the JS UI's refreshGridpoolDrilldown cadence.
    // The auto-select fires the first time the list arrives so the
    // page never opens onto an empty drill-down.
    leptos::task::spawn_local(async move {
        loop {
            if let Some(list) = fetch_pools().await {
                if selected.get_untracked().is_none()
                    && let Some(best) = list.iter().max_by_key(|p| p.trades)
                {
                    set_selected.set(Some(best.id));
                }
                set_pools.set(list);
                set_loaded.set(true);
            }
            TimeoutFuture::new(GRIDPOOL_POLL.as_millis() as u32).await;
        }
    });

    let pool_list = move || {
        let list = pools.get();
        if list.is_empty() {
            return view! {
                <i class="muted">
                    {move || if loaded.get() { "no gridpools registered" } else { "loading…" }}
                </i>
            }
            .into_any();
        }
        let sel = selected.get();
        list.into_iter()
            .map(|g| {
                let id = g.id;
                let is_sel = sel == Some(id);
                let cls = if is_sel { "row-item selected" } else { "row-item" };
                let badges = g
                    .areas
                    .iter()
                    .map(|a| view! { <span class="area-badge">{area_tag(a)}</span> })
                    .collect_view();
                let on_click = move |_| set_selected.set(Some(id));
                view! {
                    <div class=cls on:click=on_click>
                        <div class="row-head">
                            <span class="area-badge">{id.to_string()}</span>
                            <span>{g.name}</span>
                        </div>
                        <div class="row-meta muted">
                            {badges}
                            <span class="row-sep">"·"</span>
                            <span>{format!("{} orders", g.orders)}</span>
                            <span class="row-sep">"·"</span>
                            <span>{format!("{} trades", g.trades)}</span>
                        </div>
                    </div>
                }
                .into_any()
            })
            .collect_view()
            .into_any()
    };

    view! {
        <section class="panel panel-gridpools">
            <h2>"Gridpools"</h2>
            <div class="gridpool-layout">
                <div id="gridpool-list" class="gridpool-list">
                    {pool_list}
                </div>
                <div class="gridpool-orders">
                    <i class="muted">"orders pane — porting"</i>
                </div>
                <div class="gridpool-trades">
                    <i class="muted">"trades pane — porting"</i>
                </div>
            </div>
        </section>
    }
}
