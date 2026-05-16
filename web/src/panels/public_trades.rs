//! Public-trade tape — reads the WS-fed `Vec<PublicTrade>` from
//! context and renders the last `TRADES_DISPLAY_CAP` prints,
//! filtered by the area chips and an optional delivery dropdown.

use leptos::prelude::*;

use crate::intl::{short_time, short_time_sec};
use crate::panels::filter_bar::FilterState;
use crate::types::PublicTrade;
use crate::util::area_tag;

/// Recent-trades ring cap. The chart resampler reads out of the same
/// buffer so size needs to cover the longest chart window at the
/// matcher's print rate.
pub const TRADES_BUFFER_CAP: usize = 500;
const TRADES_DISPLAY_CAP: usize = 10;
const KEY_FILTER: &str = "tradingsim-trades-filter";

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn load_period_filter() -> Option<String> {
    let raw = local_storage()?.get_item(KEY_FILTER).ok().flatten()?;
    if raw.is_empty() || raw == "all" { None } else { Some(raw) }
}

fn save_period_filter(p: Option<&str>) {
    if let Some(ls) = local_storage() {
        match p {
            Some(v) => {
                let _ = ls.set_item(KEY_FILTER, v);
            }
            None => {
                let _ = ls.remove_item(KEY_FILTER);
            }
        }
    }
}

#[component]
pub fn PublicTrades() -> impl IntoView {
    let trades = expect_context::<ReadSignal<Vec<PublicTrade>>>();
    let filter = expect_context::<RwSignal<FilterState>>();
    let sim_tz = expect_context::<RwSignal<String>>();
    let focused_period = expect_context::<RwSignal<Option<String>>>();
    let period_filter = RwSignal::new(load_period_filter());

    // Drop a stale filter when the contract leaves the buffer
    // (closed + pruned by the host's snapshot ring).
    Effect::new(move |_| {
        let alive = trades.with(|v| {
            period_filter
                .get_untracked()
                .map(|p| v.iter().any(|t| t.period == p))
                .unwrap_or(true)
        });
        if !alive {
            period_filter.set(None);
            save_period_filter(None);
        }
    });

    let on_period_change = move |ev| {
        let v = event_target_value(&ev);
        let next = if v == "all" || v.is_empty() { None } else { Some(v) };
        period_filter.set(next.clone());
        save_period_filter(next.as_deref());
    };

    let period_options = move || {
        let cur = period_filter.get();
        let tz = sim_tz.get();
        let mut periods: Vec<String> =
            trades.with(|v| v.iter().map(|t| t.period.clone()).collect());
        periods.sort();
        periods.dedup();
        let mut opts: Vec<_> = vec![
            view! {
                <option value="all" selected=cur.is_none()>"All delivery periods"</option>
            }
            .into_any(),
        ];
        for p in periods {
            let selected = cur.as_deref() == Some(&p);
            let label = short_time(&p, &tz);
            opts.push(
                view! { <option value=p.clone() selected=selected>{label}</option> }.into_any(),
            );
        }
        opts.into_iter().collect_view()
    };

    let body = move || {
        let active = filter.with(|f| f.active_areas.clone());
        let pinned = period_filter.get();
        let focused = focused_period.get();
        let tz = sim_tz.get();
        let rows: Vec<_> = trades.with(|v| {
            v.iter()
                .filter(|t| {
                    if let Some(p) = pinned.as_deref()
                        && t.period != p
                    {
                        return false;
                    }
                    if let Some(p) = focused.as_deref()
                        && t.period != p
                    {
                        return false;
                    }
                    active.contains(t.buy_area.as_str())
                        || active.contains(t.sell_area.as_str())
                })
                .take(TRADES_DISPLAY_CAP)
                .cloned()
                .collect()
        });
        if rows.is_empty() {
            let msg = if pinned.is_some() || focused.is_some() {
                "no prints for this delivery"
            } else {
                "no prints for the active areas"
            };
            return view! {
                <tr><td colspan="6" class="muted"><i>{msg}</i></td></tr>
            }
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
                // Click the row to pin the chart + this panel to that
                // contract; clicking again on the same period clears
                // the pin (matches JS UI's setPeriodFilter toggle).
                let row_period = t.period.clone();
                let on_click = move |_| {
                    focused_period.update(|cur| {
                        *cur = match cur.as_deref() {
                            Some(p) if p == row_period => None,
                            _ => Some(row_period.clone()),
                        };
                    });
                };
                view! {
                    <tr on:click=on_click>
                        <td>{format!("#{}", t.id)}</td>
                        <td>{t.quantity}</td>
                        <td>{t.price}</td>
                        {area_cell}
                        <td class="muted">{short_time(&t.period, &tz)}</td>
                        <td class="muted">{short_time_sec(&t.execution_time, &tz)}</td>
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
                <span class="muted">"delivery "
                    <select on:change=on_period_change>{period_options}</select>
                </span>
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
