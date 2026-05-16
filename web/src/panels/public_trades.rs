//! Public-trade tape — reads the WS-fed `Vec<PublicTrade>` from
//! context and renders the last `TRADES_DISPLAY_CAP` prints.

use leptos::prelude::*;

use crate::panels::filter_bar::FilterState;
use crate::types::PublicTrade;
use crate::util::{area_tag, short_time_sec_utc, short_time_utc};

/// Recent-trades ring cap. The chart resampler will read out of the
/// same buffer once it ports, so size needs to cover the longest
/// chart window (~hours) at the matcher's print rate.
pub const TRADES_BUFFER_CAP: usize = 500;
const TRADES_DISPLAY_CAP: usize = 10;

#[component]
pub fn PublicTrades() -> impl IntoView {
    let trades = expect_context::<ReadSignal<Vec<PublicTrade>>>();
    let filter = expect_context::<RwSignal<FilterState>>();

    let body = move || {
        let active = filter.with(|f| f.active_areas.clone());
        let rows: Vec<_> = trades.with(|v| {
            v.iter()
                .filter(|t| {
                    active.contains(t.buy_area.as_str())
                        || active.contains(t.sell_area.as_str())
                })
                .take(TRADES_DISPLAY_CAP)
                .cloned()
                .collect()
        });
        if rows.is_empty() {
            return view! {
                <tr>
                    <td colspan="6" class="muted">
                        <i>"no prints for the active areas"</i>
                    </td>
                </tr>
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
                view! {
                    <tr>
                        <td>{format!("#{}", t.id)}</td>
                        <td>{t.quantity}</td>
                        <td>{t.price}</td>
                        {area_cell}
                        <td class="muted">{short_time_utc(&t.period)}</td>
                        <td class="muted">{short_time_sec_utc(&t.execution_time)}</td>
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
