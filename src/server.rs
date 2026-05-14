//! gRPC service implementation. The server owns a shared
//! `Arc<Mutex<World>>` and handles RPCs by briefly locking the world
//! to mutate state, then releasing the lock before any await on a
//! channel. Streams hold only their broadcast receiver across awaits.

use std::pin::Pin;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::proto::trading::{
    CancelAllGridpoolOrdersRequest, CancelAllGridpoolOrdersResponse, CancelGridpoolOrderRequest,
    CancelGridpoolOrderResponse, CreateGridpoolOrderRequest, CreateGridpoolOrderResponse,
    GetGridpoolOrderRequest, GetGridpoolOrderResponse, ListGridpoolOrdersRequest,
    ListGridpoolOrdersResponse, ListGridpoolTradesRequest, ListGridpoolTradesResponse,
    ReceiveGridpoolOrdersStreamRequest, ReceiveGridpoolOrdersStreamResponse,
    ReceiveGridpoolTradesStreamRequest, ReceiveGridpoolTradesStreamResponse,
    ReceivePublicOrderBookStreamRequest, ReceivePublicOrderBookStreamResponse,
    ReceivePublicTradesStreamRequest, ReceivePublicTradesStreamResponse, UpdateGridpoolOrderRequest,
    UpdateGridpoolOrderResponse, electricity_trading_service_server::ElectricityTradingService,
};
use crate::proto_conv::ConvError;
use crate::sim::order::{GridpoolId, Order, OrderId};
use crate::sim::world::{SubmitError, World};

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
        _request: Request<ListGridpoolOrdersRequest>,
    ) -> Result<Response<ListGridpoolOrdersResponse>, Status> {
        Err(Status::unimplemented("list_gridpool_orders: Phase 4.4c"))
    }

    type ReceiveGridpoolOrdersStreamStream = BoxStream<ReceiveGridpoolOrdersStreamResponse>;

    async fn receive_gridpool_orders_stream(
        &self,
        _request: Request<ReceiveGridpoolOrdersStreamRequest>,
    ) -> Result<Response<Self::ReceiveGridpoolOrdersStreamStream>, Status> {
        Err(Status::unimplemented(
            "receive_gridpool_orders_stream: Phase 4.4d",
        ))
    }

    async fn list_gridpool_trades(
        &self,
        _request: Request<ListGridpoolTradesRequest>,
    ) -> Result<Response<ListGridpoolTradesResponse>, Status> {
        Err(Status::unimplemented("list_gridpool_trades: Phase 7"))
    }

    type ReceiveGridpoolTradesStreamStream = BoxStream<ReceiveGridpoolTradesStreamResponse>;

    async fn receive_gridpool_trades_stream(
        &self,
        _request: Request<ReceiveGridpoolTradesStreamRequest>,
    ) -> Result<Response<Self::ReceiveGridpoolTradesStreamStream>, Status> {
        Err(Status::unimplemented(
            "receive_gridpool_trades_stream: Phase 7",
        ))
    }

    type ReceivePublicTradesStreamStream = BoxStream<ReceivePublicTradesStreamResponse>;

    async fn receive_public_trades_stream(
        &self,
        _request: Request<ReceivePublicTradesStreamRequest>,
    ) -> Result<Response<Self::ReceivePublicTradesStreamStream>, Status> {
        Err(Status::unimplemented(
            "receive_public_trades_stream: Phase 8",
        ))
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
