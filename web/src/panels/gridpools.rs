//! Gridpool drill-down (3-pane master-detail): pool list, orders
//! for the selected pool, and trades for the selected order. All
//! three are driven by signals scoped to the Gridpools component;
//! later panes get wired in as the port lands them.

use std::time::Duration;

use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::intl::{short_time, short_time_sec};
use crate::types::{GridpoolOrder, GridpoolResp, GridpoolTrade};
use crate::util::{area_tag, short_order_state, short_side, short_trade_state};

const GRIDPOOL_POLL: Duration = Duration::from_secs(3);

async fn fetch_pools() -> Option<Vec<GridpoolResp>> {
    Request::get("/api/gridpools").send().await.ok()?.json().await.ok()
}

async fn fetch_orders(pool_id: u64) -> Option<Vec<GridpoolOrder>> {
    let url = format!("/api/gridpools/{pool_id}/orders");
    Request::get(&url).send().await.ok()?.json().await.ok()
}

async fn fetch_trades(pool_id: u64, order_id: u64) -> Option<Vec<GridpoolTrade>> {
    let url = format!("/api/gridpools/{pool_id}/orders/{order_id}/trades");
    Request::get(&url).send().await.ok()?.json().await.ok()
}

#[component]
pub fn Gridpools() -> impl IntoView {
    let sim_tz = expect_context::<RwSignal<String>>();
    let (pools, set_pools) = signal(Vec::<GridpoolResp>::new());
    let (selected, set_selected) = signal(None::<u64>);
    let (orders, set_orders) = signal(Vec::<GridpoolOrder>::new());
    let (period_filter, set_period_filter) = signal(None::<String>);
    let (selected_order, set_selected_order) = signal(None::<u64>);
    let (trades, set_trades) = signal(Vec::<GridpoolTrade>::new());
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

    let refresh_trades = move || {
        let (Some(pool), Some(order)) =
            (selected.get_untracked(), selected_order.get_untracked())
        else {
            set_trades.set(Vec::new());
            return;
        };
        leptos::task::spawn_local(async move {
            if let Some(list) = fetch_trades(pool, order).await {
                set_trades.set(list);
            }
        });
    };

    // Selection-change immediate fetch so the orders pane updates
    // without waiting for the next poll tick.
    Effect::new(move |_| {
        selected.track();
        // Reset child state when the parent pool changes.
        set_selected_order.set(None);
        set_period_filter.set(None);
        refresh_orders();
    });

    Effect::new(move |_| {
        selected_order.track();
        refresh_trades();
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
        // Drop a selected order that's vanished from the list
        // (filled + pruned, or cancelled) so the trades pane doesn't
        // keep showing stale fills.
        let order_alive = orders.with(|os| {
            selected_order
                .get_untracked()
                .map(|oid| os.iter().any(|o| o.id == oid))
                .unwrap_or(true)
        });
        if !order_alive {
            set_selected_order.set(None);
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
            refresh_trades();
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
        let tz = sim_tz.get();
        let mut periods: Vec<String> =
            orders.with(|os| os.iter().map(|o| o.period.clone()).collect());
        periods.sort();
        periods.dedup();
        let cur = period_filter.get();
        let mut opts: Vec<_> = vec![view! { <option value="">"all"</option> }.into_any()];
        for p in periods {
            let selected = cur.as_deref() == Some(&p);
            let label = short_time(&p, &tz);
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
        let tz = sim_tz.get();
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
        let cur_order = selected_order.get();
        let rows = visible.into_iter().map(|o| {
            let side_cls = if o.side == "MARKET_SIDE_BUY" { "buy" } else { "sell" };
            let oid = o.id;
            let cls = if cur_order == Some(oid) {
                "gp-order-row selected"
            } else {
                "gp-order-row"
            };
            let on_click = move |_| set_selected_order.set(Some(oid));
            view! {
                <tr class=cls on:click=on_click>
                    <td>{o.id.to_string()}</td>
                    <td class=side_cls>{short_side(&o.side).to_string()}</td>
                    <td><span class="area-badge">{area_tag(&o.area)}</span></td>
                    <td>{short_time(&o.period, &tz)}</td>
                    <td>{o.price}</td>
                    <td>{format!("{}/{}", o.filled_quantity, o.quantity)}</td>
                    <td>{short_order_state(&o.state).to_string()}</td>
                    <td>{short_time_sec(&o.create_time, &tz)}</td>
                    <td>{short_time_sec(&o.modification_time, &tz)}</td>
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

    let trades_body = move || {
        if selected.get().is_none() {
            return view! { <i class="muted">"select a gridpool"</i> }.into_any();
        }
        if selected_order.get().is_none() {
            return view! { <i class="muted">"select an order"</i> }.into_any();
        }
        let list = trades.get();
        if list.is_empty() {
            return view! { <i class="muted">"no trades yet for this order"</i> }.into_any();
        }
        let tz = sim_tz.get();
        let rows = list
            .into_iter()
            .map(|t| {
                view! {
                    <tr>
                        <td>{t.id.to_string()}</td>
                        <td><span class="area-badge">{area_tag(&t.area)}</span></td>
                        <td>{short_time_sec(&t.execution_time, &tz)}</td>
                        <td>{t.price}</td>
                        <td>{t.quantity}</td>
                        <td>{short_trade_state(&t.state).to_string()}</td>
                    </tr>
                }
            })
            .collect_view();
        view! {
            <div class="scroll">
                <table>
                    <thead><tr>
                        <th>"id"</th><th>"area"</th><th>"exec"</th>
                        <th>"price"</th><th>"qty"</th><th>"state"</th>
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
                            <select id="gridpool-period-select" on:change=on_period_change>
                                {period_options}
                            </select>
                        </span>
                    </div>
                    <div id="gridpool-orders">{orders_body}</div>
                </div>
                <div class="gridpool-trades">
                    <h2>"Trades"</h2>
                    <div id="gridpool-trades">{trades_body}</div>
                </div>
            </div>
        </section>
    }
}
