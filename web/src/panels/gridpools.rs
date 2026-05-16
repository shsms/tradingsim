//! Gridpool drill-down (3-pane master-detail): pool list, orders
//! for the selected pool, and trades for the selected order. All
//! three are driven by signals scoped to the Gridpools component;
//! later panes get wired in as the port lands them.

use std::time::Duration;

use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::types::{GridpoolOrder, GridpoolResp};
use crate::util::{area_tag, short_order_state, short_side, short_time_sec_utc, short_time_utc};

const GRIDPOOL_POLL: Duration = Duration::from_secs(3);

async fn fetch_pools() -> Option<Vec<GridpoolResp>> {
    Request::get("/api/gridpools").send().await.ok()?.json().await.ok()
}

async fn fetch_orders(pool_id: u64) -> Option<Vec<GridpoolOrder>> {
    let url = format!("/api/gridpools/{pool_id}/orders");
    Request::get(&url).send().await.ok()?.json().await.ok()
}

#[component]
pub fn Gridpools() -> impl IntoView {
    let (pools, set_pools) = signal(Vec::<GridpoolResp>::new());
    let (selected, set_selected) = signal(None::<u64>);
    let (orders, set_orders) = signal(Vec::<GridpoolOrder>::new());
    let (period_filter, set_period_filter) = signal(None::<String>);
    let (loaded, set_loaded) = signal(false);

    let refresh_orders = move || {
        let Some(id) = selected.get_untracked() else {
            set_orders.set(Vec::new());
            return;
        };
        leptos::task::spawn_local(async move {
            if let Some(list) = fetch_orders(id).await {
                set_orders.set(list);
            }
        });
    };

    // Selection-change immediate fetch so the orders pane updates
    // without waiting for the next poll tick.
    Effect::new(move |_| {
        selected.track();
        refresh_orders();
    });

    // Drop a stale period filter whenever the orders list changes —
    // the pinned contract may have closed between fetches.
    Effect::new(move |_| {
        let active = orders.with(|os| {
            period_filter
                .get_untracked()
                .map(|p| os.iter().any(|o| o.period == p))
                .unwrap_or(true)
        });
        if !active {
            set_period_filter.set(None);
        }
    });

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
            refresh_orders();
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

    let period_options = move || {
        let mut periods: Vec<String> =
            orders.with(|os| os.iter().map(|o| o.period.clone()).collect());
        periods.sort();
        periods.dedup();
        let cur = period_filter.get();
        let mut opts: Vec<_> = vec![view! { <option value="">"all"</option> }.into_any()];
        for p in periods {
            let selected = cur.as_deref() == Some(&p);
            let label = short_time_utc(&p);
            let value = p;
            opts.push(
                view! {
                    <option value=value.clone() selected=selected>
                        {label}
                    </option>
                }
                .into_any(),
            );
        }
        opts.into_iter().collect_view()
    };

    let on_period_change = move |ev| {
        let v = event_target_value(&ev);
        set_period_filter.set(if v.is_empty() { None } else { Some(v) });
    };

    let orders_body = move || {
        if selected.get().is_none() {
            return view! { <i class="muted">"select a gridpool"</i> }.into_any();
        }
        let filter = period_filter.get();
        let visible: Vec<_> = orders.with(|os| {
            os.iter()
                .filter(|o| filter.as_deref().is_none_or(|p| o.period == p))
                .cloned()
                .collect()
        });
        if visible.is_empty() {
            let msg = if filter.is_some() {
                "no orders for this delivery"
            } else {
                "no orders for this gridpool"
            };
            return view! { <i class="muted">{msg}</i> }.into_any();
        }
        let rows = visible.into_iter().map(|o| {
            let side_cls = if o.side == "MARKET_SIDE_BUY" { "buy" } else { "sell" };
            view! {
                <tr class="gp-order-row">
                    <td>{o.id.to_string()}</td>
                    <td class=side_cls>{short_side(&o.side).to_string()}</td>
                    <td><span class="area-badge">{area_tag(&o.area)}</span></td>
                    <td>{short_time_utc(&o.period)}</td>
                    <td>{o.price}</td>
                    <td>{format!("{}/{}", o.filled_quantity, o.quantity)}</td>
                    <td>{short_order_state(&o.state).to_string()}</td>
                    <td>{short_time_sec_utc(&o.create_time)}</td>
                    <td>{short_time_sec_utc(&o.modification_time)}</td>
                </tr>
            }
        }).collect_view();
        view! {
            <div class="scroll">
                <table>
                    <thead><tr>
                        <th>"id"</th><th>"side"</th><th>"area"</th><th>"delivery"</th>
                        <th>"price"</th><th>"filled/qty"</th><th>"state"</th>
                        <th>"created"</th><th>"upd"</th>
                    </tr></thead>
                    <tbody>{rows}</tbody>
                </table>
            </div>
        }
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
                    <div class="book-head">
                        <h2>"Orders"</h2>
                        <span class="muted">"delivery "
                            <select on:change=on_period_change>{period_options}</select>
                        </span>
                    </div>
                    {orders_body}
                </div>
                <div class="gridpool-trades">
                    <i class="muted">"trades pane — porting"</i>
                </div>
            </div>
        </section>
    }
}
