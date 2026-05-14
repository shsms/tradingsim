//! gRPC service implementation. The server owns a shared
//! `Arc<Mutex<World>>` and handles RPCs by briefly locking the world
//! to mutate state, then releasing the lock before any await on a
//! channel. Streams hold only their broadcast receiver across awaits.

use std::pin::Pin;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::proto::common::pagination::{
    PaginationInfo, PaginationParams, pagination_params::Params as PaginationOneof,
};
use crate::proto::trading::{
    self as proto_trading, CancelAllGridpoolOrdersRequest, CancelAllGridpoolOrdersResponse,
    CancelGridpoolOrderRequest, CancelGridpoolOrderResponse, CreateGridpoolOrderRequest,
    CreateGridpoolOrderResponse, DeliveryTimeFilter, GetGridpoolOrderRequest,
    GetGridpoolOrderResponse, GridpoolOrderFilter, GridpoolTradeFilter, ListGridpoolOrdersRequest,
    ListGridpoolOrdersResponse, ListGridpoolTradesRequest, ListGridpoolTradesResponse,
    PublicTradeFilter, ReceiveGridpoolOrdersStreamRequest, ReceiveGridpoolOrdersStreamResponse,
    ReceiveGridpoolTradesStreamRequest, ReceiveGridpoolTradesStreamResponse,
    ReceivePublicOrderBookStreamRequest, ReceivePublicOrderBookStreamResponse,
    ReceivePublicTradesStreamRequest, ReceivePublicTradesStreamResponse, UpdateGridpoolOrderRequest,
    UpdateGridpoolOrderResponse, electricity_trading_service_server::ElectricityTradingService,
};
use crate::proto_conv::{ConvError, timestamp_from_proto};
use crate::sim::market::DeliveryPeriod;
use crate::sim::order::{GridpoolId, Order, OrderDetail, OrderId};
use crate::sim::trade::{PublicTrade, Trade};
use crate::sim::world::{SubmitError, World};

/// Cap and default for page sizes. Clients can ask for less, but
/// never more than the cap.
const DEFAULT_PAGE_SIZE: u32 = 200;
const MAX_PAGE_SIZE: u32 = 1000;

/// Boxed-stream alias used by every server-streaming RPC's
/// associated type. `Status` errors from the stream become per-item
/// `Err` in the gRPC frame.
type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[derive(Clone)]
pub struct ElectricityTradingServer {
    world: Arc<Mutex<World>>,
}

impl ElectricityTradingServer {
    pub fn new(world: Arc<Mutex<World>>) -> Self {
        Self { world }
    }
}

/// Map a sim-side SubmitError onto the closest gRPC Status code.
/// NOT_FOUND for gridpool/order misses, INVALID_ARGUMENT for any
/// validation rejection — the proto's STATE_REASON_VALIDATION_FAIL
/// covers all the latter on the wire.
fn submit_err_to_status(e: SubmitError) -> Status {
    use SubmitError::*;
    match e {
        UnknownGridpool => Status::not_found("unknown gridpool"),
        OrderNotFound => Status::not_found("order not found"),
        OrderAlreadyTerminal => Status::failed_precondition("order already terminal"),
        e => Status::invalid_argument(format!("validation: {e:?}")),
    }
}

fn conv_err_to_status(e: ConvError) -> Status {
    Status::invalid_argument(format!("proto conversion: {e}"))
}

/// Parse PaginationParams.params (oneof) into (page_size, cursor).
/// Cursor is an opaque u64 the caller wraps in its id newtype.
/// First-page calls carry PageSize; continuations carry PageToken,
/// which we encode as "page_size:last_id". Empty token = first page.
fn parse_pagination(p: &Option<PaginationParams>) -> Result<(u32, Option<u64>), Status> {
    let params = match p.as_ref().and_then(|p| p.params.as_ref()) {
        Some(p) => p,
        None => return Ok((DEFAULT_PAGE_SIZE, None)),
    };
    match params {
        PaginationOneof::PageSize(sz) => {
            let n = if *sz == 0 { DEFAULT_PAGE_SIZE } else { *sz }.min(MAX_PAGE_SIZE);
            Ok((n, None))
        }
        PaginationOneof::PageToken(tok) => {
            let (sz, last) = tok.split_once(':').ok_or_else(|| {
                Status::invalid_argument("malformed page_token (want \"size:last_id\")")
            })?;
            let page_size: u32 = sz
                .parse()
                .map_err(|_| Status::invalid_argument("bad page_size in page_token"))?;
            let last_id: u64 = last
                .parse()
                .map_err(|_| Status::invalid_argument("bad cursor in page_token"))?;
            Ok((page_size.min(MAX_PAGE_SIZE), Some(last_id)))
        }
    }
}

fn encode_page_token(page_size: u32, last_id: u64) -> String {
    format!("{page_size}:{last_id}")
}

/// True iff a DeliveryTimeFilter selects `period`. Half-open
/// interval semantics on period.start; duration_filters is an
/// allow-list (empty = any).
fn period_matches_dtf(period: DeliveryPeriod, dtf: &DeliveryTimeFilter) -> bool {
    if let Some(iv) = dtf.time_interval {
        if let Some(ts) = iv.start_time.as_ref() {
            if timestamp_from_proto(ts).map(|t| period.start < t).unwrap_or(true) {
                return false;
            }
        }
        if let Some(ts) = iv.end_time.as_ref() {
            if timestamp_from_proto(ts).map(|t| period.start >= t).unwrap_or(true) {
                return false;
            }
        }
    }
    if !dtf.duration_filters.is_empty() {
        let d_i32 = crate::proto::common::grid::DeliveryDuration::from(period.duration) as i32;
        if !dtf.duration_filters.contains(&d_i32) {
            return false;
        }
    }
    true
}

fn matches_gridpool_trade_filter(t: &Trade, f: &GridpoolTradeFilter) -> bool {
    if !f.states.is_empty() {
        let s = proto_trading::TradeState::from(t.state) as i32;
        if !f.states.contains(&s) {
            return false;
        }
    }
    if !f.trade_ids.is_empty() && !f.trade_ids.contains(&t.id.0) {
        return false;
    }
    if let Some(want) = f.side {
        let s = proto_trading::MarketSide::from(t.side) as i32;
        if s != want {
            return false;
        }
    }
    if let Some(dtf) = f.delivery_time_filter.as_ref() {
        if !period_matches_dtf(t.period, dtf) {
            return false;
        }
    }
    if let Some(area) = f.delivery_area.as_ref() {
        if area.code != t.area.code {
            return false;
        }
    }
    // `tag` filter requires an order lookup; deferred.
    true
}

fn matches_public_trade_filter(t: &PublicTrade, f: &PublicTradeFilter) -> bool {
    if !f.states.is_empty() {
        let s = proto_trading::TradeState::from(t.state) as i32;
        if !f.states.contains(&s) {
            return false;
        }
    }
    if let Some(want_period) = f.delivery_period.as_ref() {
        let want_secs = want_period.start.as_ref().map(|ts| ts.seconds);
        if want_secs != Some(t.period.start.timestamp()) {
            return false;
        }
        let want_duration = want_period.duration;
        let got =
            crate::proto::common::grid::DeliveryDuration::from(t.period.duration) as i32;
        if want_duration != got {
            return false;
        }
    }
    if let Some(area) = f.buy_delivery_area.as_ref() {
        if area.code != t.buy_area.code {
            return false;
        }
    }
    if let Some(area) = f.sell_delivery_area.as_ref() {
        if area.code != t.sell_area.code {
            return false;
        }
    }
    true
}

fn matches_order_filter(d: &OrderDetail, f: &GridpoolOrderFilter) -> bool {
    if !f.states.is_empty() {
        let s_i32 = proto_trading::OrderState::from(d.state.state) as i32;
        if !f.states.contains(&s_i32) {
            return false;
        }
    }
    if let Some(want) = f.side {
        let s_i32 = proto_trading::MarketSide::from(d.order.side) as i32;
        if s_i32 != want {
            return false;
        }
    }
    if let Some(dtf) = f.delivery_time_filter.as_ref() {
        if !period_matches_dtf(d.order.period, dtf) {
            return false;
        }
    }
    if let Some(area) = f.delivery_area.as_ref() {
        if area.code != d.order.area.code {
            return false;
        }
    }
    if let Some(want_tag) = f.tag.as_deref() {
        if d.order.tag.as_deref() != Some(want_tag) {
            return false;
        }
    }
    if !f.order_ids.is_empty() && !f.order_ids.contains(&d.id.0) {
        return false;
    }
    true
}

#[tonic::async_trait]
impl ElectricityTradingService for ElectricityTradingServer {
    async fn create_gridpool_order(
        &self,
        request: Request<CreateGridpoolOrderRequest>,
    ) -> Result<Response<CreateGridpoolOrderResponse>, Status> {
        let req = request.into_inner();
        let gridpool_id = GridpoolId(req.gridpool_id);
        let order_proto = req
            .order
            .ok_or_else(|| Status::invalid_argument("missing order"))?;
        let order = Order::try_from(&order_proto).map_err(conv_err_to_status)?;

        let detail = {
            let mut w = self.world.lock().await;
            w.submit_order(gridpool_id, order, Utc::now())
                .map_err(submit_err_to_status)?
        };
        Ok(Response::new(CreateGridpoolOrderResponse {
            gridpool_id: gridpool_id.0,
            order_detail: Some((&detail).into()),
        }))
    }

    async fn update_gridpool_order(
        &self,
        _request: Request<UpdateGridpoolOrderRequest>,
    ) -> Result<Response<UpdateGridpoolOrderResponse>, Status> {
        Err(Status::unimplemented("update_gridpool_order: Phase 6"))
    }

    async fn cancel_gridpool_order(
        &self,
        request: Request<CancelGridpoolOrderRequest>,
    ) -> Result<Response<CancelGridpoolOrderResponse>, Status> {
        let req = request.into_inner();
        let gridpool_id = GridpoolId(req.gridpool_id);
        let order_id = OrderId(req.order_id);
        let detail = {
            let mut w = self.world.lock().await;
            w.cancel_order(gridpool_id, order_id, Utc::now())
                .map_err(submit_err_to_status)?
        };
        Ok(Response::new(CancelGridpoolOrderResponse {
            gridpool_id: gridpool_id.0,
            order_detail: Some((&detail).into()),
        }))
    }

    async fn cancel_all_gridpool_orders(
        &self,
        request: Request<CancelAllGridpoolOrdersRequest>,
    ) -> Result<Response<CancelAllGridpoolOrdersResponse>, Status> {
        let gridpool_id = GridpoolId(request.into_inner().gridpool_id);
        let mut w = self.world.lock().await;
        // Snapshot the non-terminal ids first; cancel_order is &mut
        // on the World and would alias the iterator otherwise.
        let ids: Vec<OrderId> = w
            .gridpools()
            .get(gridpool_id)
            .ok_or_else(|| Status::not_found("unknown gridpool"))?
            .orders()
            .filter(|d| !d.state.state.is_terminal())
            .map(|d| d.id)
            .collect();
        for id in ids {
            // Skip terminal-flips that race past us (e.g. matcher just
            // filled an order between snapshot + cancel); the
            // OrderAlreadyTerminal path is benign.
            let _ = w.cancel_order(gridpool_id, id, Utc::now());
        }
        Ok(Response::new(CancelAllGridpoolOrdersResponse {
            gridpool_id: gridpool_id.0,
        }))
    }

    async fn get_gridpool_order(
        &self,
        request: Request<GetGridpoolOrderRequest>,
    ) -> Result<Response<GetGridpoolOrderResponse>, Status> {
        let req = request.into_inner();
        let gridpool_id = GridpoolId(req.gridpool_id);
        let order_id = OrderId(req.order_id);
        let w = self.world.lock().await;
        let gp = w
            .gridpools()
            .get(gridpool_id)
            .ok_or_else(|| Status::not_found("unknown gridpool"))?;
        let detail = gp
            .get_order(order_id)
            .ok_or_else(|| Status::not_found("order not found"))?;
        Ok(Response::new(GetGridpoolOrderResponse {
            gridpool_id: gridpool_id.0,
            order_detail: Some(detail.into()),
        }))
    }

    async fn list_gridpool_orders(
        &self,
        request: Request<ListGridpoolOrdersRequest>,
    ) -> Result<Response<ListGridpoolOrdersResponse>, Status> {
        let req = request.into_inner();
        let gridpool_id = GridpoolId(req.gridpool_id);
        let (page_size, cursor) = parse_pagination(&req.pagination_params)?;
        let filter = req.filter.unwrap_or_default();

        let w = self.world.lock().await;
        let gp = w
            .gridpools()
            .get(gridpool_id)
            .ok_or_else(|| Status::not_found("unknown gridpool"))?;

        let mut all: Vec<&OrderDetail> = gp
            .orders()
            .filter(|d| matches_order_filter(d, &filter))
            .collect();
        all.sort_by_key(|d| d.id);

        let start = match cursor {
            Some(c) => all.iter().position(|d| d.id.0 > c).unwrap_or(all.len()),
            None => 0,
        };
        let end = (start + page_size as usize).min(all.len());
        let page = &all[start..end];
        let next = if end < all.len() {
            page.last().map(|d| encode_page_token(page_size, d.id.0))
        } else {
            None
        };

        let resp = ListGridpoolOrdersResponse {
            order_details: page.iter().map(|d| (*d).into()).collect(),
            pagination_info: Some(PaginationInfo {
                total_items: all.len() as u32,
                next_page_token: next,
            }),
        };
        Ok(Response::new(resp))
    }

    type ReceiveGridpoolOrdersStreamStream = BoxStream<ReceiveGridpoolOrdersStreamResponse>;

    async fn receive_gridpool_orders_stream(
        &self,
        request: Request<ReceiveGridpoolOrdersStreamRequest>,
    ) -> Result<Response<Self::ReceiveGridpoolOrdersStreamStream>, Status> {
        let req = request.into_inner();
        let gridpool_id = GridpoolId(req.gridpool_id);
        let filter = req.filter.unwrap_or_default();

        // Subscribe under the lock; release before consuming the stream
        // so other RPCs aren't held up by long-lived subscribers.
        let receiver = {
            let w = self.world.lock().await;
            w.subscribe_orders(gridpool_id)
                .ok_or_else(|| Status::not_found("unknown gridpool"))?
        };

        let stream = BroadcastStream::new(receiver).filter_map(move |item| match item {
            Ok(detail) if matches_order_filter(&detail, &filter) => {
                Some(Ok(ReceiveGridpoolOrdersStreamResponse {
                    order_detail: Some((&detail).into()),
                }))
            }
            // Drop both non-matching items and Lagged errors silently —
            // the stream contract is best-effort.
            _ => None,
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_gridpool_trades(
        &self,
        request: Request<ListGridpoolTradesRequest>,
    ) -> Result<Response<ListGridpoolTradesResponse>, Status> {
        let req = request.into_inner();
        let gridpool_id = GridpoolId(req.gridpool_id);
        let (page_size, cursor) = parse_pagination(&req.pagination_params)?;
        let filter = req.filter.unwrap_or_default();

        let w = self.world.lock().await;
        let gp = w
            .gridpools()
            .get(gridpool_id)
            .ok_or_else(|| Status::not_found("unknown gridpool"))?;
        let mut all: Vec<&Trade> = gp
            .trades()
            .iter()
            .filter(|t| matches_gridpool_trade_filter(t, &filter))
            .collect();
        all.sort_by_key(|t| t.id);

        let start = match cursor {
            Some(c) => all.iter().position(|t| t.id.0 > c).unwrap_or(all.len()),
            None => 0,
        };
        let end = (start + page_size as usize).min(all.len());
        let page = &all[start..end];
        let next = if end < all.len() {
            page.last().map(|t| encode_page_token(page_size, t.id.0))
        } else {
            None
        };
        Ok(Response::new(ListGridpoolTradesResponse {
            trades: page.iter().map(|t| (*t).into()).collect(),
            pagination_info: Some(PaginationInfo {
                total_items: all.len() as u32,
                next_page_token: next,
            }),
        }))
    }

    type ReceiveGridpoolTradesStreamStream = BoxStream<ReceiveGridpoolTradesStreamResponse>;

    async fn receive_gridpool_trades_stream(
        &self,
        request: Request<ReceiveGridpoolTradesStreamRequest>,
    ) -> Result<Response<Self::ReceiveGridpoolTradesStreamStream>, Status> {
        let req = request.into_inner();
        let gridpool_id = GridpoolId(req.gridpool_id);
        let filter = req.filter.unwrap_or_default();
        let rx = {
            let w = self.world.lock().await;
            w.subscribe_gridpool_trades(gridpool_id)
                .ok_or_else(|| Status::not_found("unknown gridpool"))?
        };
        let stream = BroadcastStream::new(rx).filter_map(move |item| match item {
            Ok(t) if matches_gridpool_trade_filter(&t, &filter) => {
                Some(Ok(ReceiveGridpoolTradesStreamResponse {
                    trade: Some((&t).into()),
                }))
            }
            _ => None,
        });
        Ok(Response::new(Box::pin(stream)))
    }

    type ReceivePublicTradesStreamStream = BoxStream<ReceivePublicTradesStreamResponse>;

    async fn receive_public_trades_stream(
        &self,
        request: Request<ReceivePublicTradesStreamRequest>,
    ) -> Result<Response<Self::ReceivePublicTradesStreamStream>, Status> {
        let req = request.into_inner();
        let filter = req.filter.unwrap_or_default();
        // start_time / end_time replay is deferred — we serve live
        // from the subscription point on. The proto's stream lifetime
        // contract allows that ("best-effort").
        let rx = self.world.lock().await.subscribe_public_trades();
        let stream = BroadcastStream::new(rx).filter_map(move |item| match item {
            Ok(t) if matches_public_trade_filter(&t, &filter) => {
                Some(Ok(ReceivePublicTradesStreamResponse {
                    public_trade: Some((&t).into()),
                }))
            }
            _ => None,
        });
        Ok(Response::new(Box::pin(stream)))
    }

    type ReceivePublicOrderBookStreamStream = BoxStream<ReceivePublicOrderBookStreamResponse>;

    async fn receive_public_order_book_stream(
        &self,
        _request: Request<ReceivePublicOrderBookStreamRequest>,
    ) -> Result<Response<Self::ReceivePublicOrderBookStreamStream>, Status> {
        Err(Status::unimplemented(
            "receive_public_order_book_stream: Phase 8",
        ))
    }
}
