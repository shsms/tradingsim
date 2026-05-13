//! gRPC service implementation. The server owns a shared
//! `Arc<Mutex<World>>` and handles RPCs by briefly locking the world
//! to mutate state, then releasing the lock before any await on a
//! channel. Streams hold only their broadcast receiver across awaits.

use std::pin::Pin;

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

/// Boxed-stream alias used by every server-streaming RPC's
/// associated type. `Status` errors from the stream become per-item
/// `Err` in the gRPC frame.
type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[derive(Default, Clone)]
pub struct ElectricityTradingServer {
    // World handle joins next commit alongside the first real RPC.
}

impl ElectricityTradingServer {
    pub fn new() -> Self {
        Self::default()
    }
}

#[tonic::async_trait]
impl ElectricityTradingService for ElectricityTradingServer {
    async fn create_gridpool_order(
        &self,
        _request: Request<CreateGridpoolOrderRequest>,
    ) -> Result<Response<CreateGridpoolOrderResponse>, Status> {
        Err(Status::unimplemented("create_gridpool_order: Phase 4.4b"))
    }

    async fn update_gridpool_order(
        &self,
        _request: Request<UpdateGridpoolOrderRequest>,
    ) -> Result<Response<UpdateGridpoolOrderResponse>, Status> {
        Err(Status::unimplemented("update_gridpool_order: Phase 6"))
    }

    async fn cancel_gridpool_order(
        &self,
        _request: Request<CancelGridpoolOrderRequest>,
    ) -> Result<Response<CancelGridpoolOrderResponse>, Status> {
        Err(Status::unimplemented("cancel_gridpool_order: Phase 4.4b"))
    }

    async fn cancel_all_gridpool_orders(
        &self,
        _request: Request<CancelAllGridpoolOrdersRequest>,
    ) -> Result<Response<CancelAllGridpoolOrdersResponse>, Status> {
        Err(Status::unimplemented(
            "cancel_all_gridpool_orders: Phase 4.4b",
        ))
    }

    async fn get_gridpool_order(
        &self,
        _request: Request<GetGridpoolOrderRequest>,
    ) -> Result<Response<GetGridpoolOrderResponse>, Status> {
        Err(Status::unimplemented("get_gridpool_order: Phase 4.4b"))
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
