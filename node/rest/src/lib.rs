// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![forbid(unsafe_code)]

#[macro_use]
extern crate tracing;

mod helpers;
pub use helpers::*;

mod routes;

use snarkos_node_consensus::Consensus;
use snarkos_node_messages::{Data, Message, UnconfirmedTransaction};
use snarkos_node_router::Routing;
use snarkvm::{
    console::{program::ProgramID, types::Field},
    prelude::{cfg_into_iter, Ledger, Network},
    synthesizer::ConsensusStorage,
};

use anyhow::Result;
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State},
    http::{header::CONTENT_TYPE, Method, Request, StatusCode},
    middleware,
    middleware::Next,
    response::Response,
    routing::{get, post},
    Json,
};
use axum_extra::response::ErasedJson;
use parking_lot::Mutex;
use std::{net::SocketAddr, sync::Arc};
use tokio::task::JoinHandle;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

/// A REST API server for the ledger.
#[derive(Clone)]
pub struct Rest<N: Network, C: ConsensusStorage<N>, R: Routing<N>> {
    /// The consensus module.
    consensus: Option<Consensus<N, C>>,
    /// The ledger.
    ledger: Ledger<N, C>,
    /// The node (routing).
    routing: Arc<R>,
    /// The server handles.
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl<N: Network, C: 'static + ConsensusStorage<N>, R: Routing<N>> Rest<N, C, R> {
    /// Initializes a new instance of the server.
    pub fn start(
        rest_ip: SocketAddr,
        consensus: Option<Consensus<N, C>>,
        ledger: Ledger<N, C>,
        routing: Arc<R>,
    ) -> Result<Self> {
        // Initialize the server.
        let mut server = Self { consensus, ledger, routing, handles: Default::default() };
        // Spawn the server.
        server.spawn_server(rest_ip);
        // Return the server.
        Ok(server)
    }
}

impl<N: Network, C: ConsensusStorage<N>, R: Routing<N>> Rest<N, C, R> {
    /// Returns the ledger.
    pub const fn ledger(&self) -> &Ledger<N, C> {
        &self.ledger
    }

    /// Returns the handles.
    pub const fn handles(&self) -> &Arc<Mutex<Vec<JoinHandle<()>>>> {
        &self.handles
    }
}

impl<N: Network, C: ConsensusStorage<N>, R: Routing<N>> Rest<N, C, R> {
    fn spawn_server(&mut self, rest_ip: SocketAddr) {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([CONTENT_TYPE]);

        let router = {
            axum::Router::new()

            // GET ../latest/..
            .route("/testnet3/latest/height", get(Self::latest_height))
            .route("/testnet3/latest/hash", get(Self::latest_hash))
            .route("/testnet3/latest/block", get(Self::latest_block))
            .route("/testnet3/latest/stateRoot", get(Self::latest_state_root))

            // GET ../block/..
            .route("/testnet3/block/:height_or_hash", get(Self::get_block))
            // The path param here is actually only the height, but the name must match the route
            // above, otherwise there'll be a conflict at runtime.
            .route("/testnet3/block/:height_or_hash/transactions", get(Self::get_block_transactions))

            // GET and POST ../transaction/..
            .route("/testnet3/transaction/:id", get(Self::get_transaction))
            .route("/testnet3/transaction/broadcast", post(Self::transaction_broadcast))

            // GET ../find/..
            .route("/testnet3/find/blockHash/:tx_id", get(Self::find_block_hash))
            .route("/testnet3/find/transactionID/deployment/:program_id", get(Self::find_transaction_id_from_program_id))
            .route("/testnet3/find/transactionID/:transition_id", get(Self::find_transaction_id_from_transition_id))
            .route("/testnet3/find/transitionID/:input_or_output_id", get(Self::find_transition_id))

            // GET ../peers/..
            .route("/testnet3/peers/count", get(Self::get_peers_count))
            .route("/testnet3/peers/all", get(Self::get_peers_all))
            .route("/testnet3/peers/all/metrics", get(Self::get_peers_all_metrics))

            // GET ../program/..
            .route("/testnet3/program/:id", get(Self::get_program))
            .route("/testnet3/program/:id/mappings", get(Self::get_mapping_names))
            .route("/testnet3/program/:id/mapping/:name/:key", get(Self::get_mapping_value))

            // GET misc endpoints.
            .route("/testnet3/blocks", get(Self::get_blocks))
            .route("/testnet3/height/:hash", get(Self::get_height))
            .route("/testnet3/memoryPool/transactions", get(Self::get_memory_pool_transactions))
            .route("/testnet3/statePath/:commitment", get(Self::get_state_path_for_commitment))
            .route("/testnet3/beacons", get(Self::get_beacons))
            .route("/testnet3/node/address", get(Self::get_node_address))
            .route("/testnet3/node/env", get(Self::get_env_info))
            .route("/testnet3/records/all/:view_key", get(Self::get_records_all))
            .route("/testnet3/records/spent/:view_key", get(Self::get_records_spent))
            .route("/testnet3/records/unspent/:view_key", get(Self::get_records_unspent))

            // Pass in `Rest` to make things convenient.
            .with_state(self.clone())
            // Enable tower-http tracing.
            .layer(TraceLayer::new_for_http())
            // Custom logging.
            .layer(middleware::from_fn(log_middleware))
            // Enable CORS.
            .layer(cors)
            // Cap body size at 10MB.
            .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
            // JWT auth.
            // .layer(middleware::from_fn(auth_middleware))
        };

        self.handles.lock().push(tokio::spawn(async move {
            axum::Server::bind(&rest_ip)
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .expect("couldn't start rest server");
        }))
    }
}

async fn log_middleware<B>(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode>
where
    B: Send,
{
    info!("Received '{} {}' from '{addr}'", request.method(), request.uri());

    Ok(next.run(request).await)
}
