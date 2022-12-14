use anyhow::Result;
use async_trait::async_trait;
use linera_base::messages::ChainId;
use linera_chain::messages::{BlockAndRound, Value};
use linera_rpc::{
    config::{ShardConfig, ValidatorInternalNetworkConfig, ValidatorPublicNetworkConfig},
    grpc_network::{
        grpc::{
            validator_node_client::ValidatorNodeClient,
            validator_node_server::{ValidatorNode, ValidatorNodeServer},
            validator_worker_client::ValidatorWorkerClient,
            validator_worker_server::{ValidatorWorker, ValidatorWorkerServer},
            ChainInfoResult,
        },
        BlockProposal, Certificate, ChainInfoQuery, CrossChainRequest,
    },
    pool::ConnectionPool,
};
use std::net::SocketAddr;
use tonic::{
    transport::{Channel, Server},
    Request, Response, Status,
};

/// Boilerplate to extract the underlying chain id, use it to get the corresponding shard
/// and forward the message.
macro_rules! proxy {
    ($self:ident, $handler:ident, $req:ident, $client:ident) => {{
        log::debug!(
            "handler [{}:{}] proxying request [{:?}] from {:?}",
            stringify!($client),
            stringify!($handler),
            $req,
            $req.remote_addr()
        );
        let inner = $req.into_inner();
        let shard = $self
            .shard_for(&inner)
            .ok_or(Status::not_found("could not find shard for message"))?;
        let mut client = $self
            .$client(&shard)
            .await
            .map_err(|_| Status::internal("could not connect to shard"))?;
        client.$handler(inner).await
    }};
}

#[derive(Clone)]
pub struct GrpcProxy {
    public_config: ValidatorPublicNetworkConfig,
    internal_config: ValidatorInternalNetworkConfig,
    node_connection_pool: ConnectionPool<ValidatorNodeClient<Channel>>,
    worker_connection_pool: ConnectionPool<ValidatorWorkerClient<Channel>>,
}

impl GrpcProxy {
    pub fn new(
        public_config: ValidatorPublicNetworkConfig,
        internal_config: ValidatorInternalNetworkConfig,
    ) -> Self {
        Self {
            public_config,
            internal_config,
            node_connection_pool: ConnectionPool::new(),
            worker_connection_pool: ConnectionPool::new(),
        }
    }

    fn as_validator_worker(&self) -> ValidatorWorkerServer<Self> {
        ValidatorWorkerServer::new(self.clone())
    }

    fn as_validator_node(&self) -> ValidatorNodeServer<Self> {
        ValidatorNodeServer::new(self.clone())
    }

    fn address(&self) -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], self.public_config.port))
    }

    fn shard_for(&self, proxyable: &impl Proxyable) -> Option<ShardConfig> {
        Some(
            self.internal_config
                .get_shard_for(proxyable.chain_id()?)
                .clone(),
        )
    }

    async fn worker_client_for_shard(
        &self,
        shard: &ShardConfig,
    ) -> Result<ValidatorWorkerClient<Channel>> {
        let address = shard.http_address();
        let client = self
            .worker_connection_pool
            .cloned_client_for_address(address)
            .await?;

        Ok(client)
    }

    async fn node_client_for_shard(
        &self,
        shard: &ShardConfig,
    ) -> Result<ValidatorNodeClient<Channel>> {
        let address = shard.http_address();
        let client = self
            .node_connection_pool
            .cloned_client_for_address(address)
            .await?;

        Ok(client)
    }

    pub async fn run(self) -> Result<()> {
        log::info!("Starting gRPC proxy on {}...", self.address());
        Ok(Server::builder()
            .add_service(self.as_validator_node())
            .add_service(self.as_validator_worker())
            .serve(self.address())
            .await?)
    }
}

#[async_trait]
impl ValidatorWorker for GrpcProxy {
    async fn handle_block_proposal(
        &self,
        request: Request<BlockProposal>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        proxy!(
            self,
            handle_block_proposal,
            request,
            worker_client_for_shard
        )
    }

    async fn handle_certificate(
        &self,
        request: Request<Certificate>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        proxy!(self, handle_certificate, request, worker_client_for_shard)
    }

    async fn handle_chain_info_query(
        &self,
        request: Request<ChainInfoQuery>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        proxy!(
            self,
            handle_chain_info_query,
            request,
            worker_client_for_shard
        )
    }

    async fn handle_cross_chain_request(
        &self,
        request: Request<CrossChainRequest>,
    ) -> Result<Response<()>, Status> {
        proxy!(
            self,
            handle_cross_chain_request,
            request,
            worker_client_for_shard
        )
    }
}

#[async_trait]
impl ValidatorNode for GrpcProxy {
    async fn handle_block_proposal(
        &self,
        request: Request<BlockProposal>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        proxy!(self, handle_block_proposal, request, node_client_for_shard)
    }

    async fn handle_certificate(
        &self,
        request: Request<Certificate>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        proxy!(self, handle_certificate, request, node_client_for_shard)
    }

    async fn handle_chain_info_query(
        &self,
        request: Request<ChainInfoQuery>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        proxy!(
            self,
            handle_chain_info_query,
            request,
            node_client_for_shard
        )
    }
}

/// Types which are proxyable and expose the appropriate methods to be handled
/// by the `GrpcProxy`
trait Proxyable {
    fn chain_id(&self) -> Option<ChainId>;
}

impl Proxyable for BlockProposal {
    fn chain_id(&self) -> Option<ChainId> {
        match bcs::from_bytes::<BlockAndRound>(&self.content) {
            Ok(block_and_round) => Some(block_and_round.block.chain_id),
            Err(_) => None,
        }
    }
}

impl Proxyable for Certificate {
    fn chain_id(&self) -> Option<ChainId> {
        match bcs::from_bytes::<Value>(&self.value) {
            Ok(value) => Some(value.chain_id()),
            Err(_) => None,
        }
    }
}

impl Proxyable for ChainInfoQuery {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl Proxyable for CrossChainRequest {
    fn chain_id(&self) -> Option<ChainId> {
        match linera_core::messages::CrossChainRequest::try_from(self.clone()) {
            Ok(cross_chain_request) => Some(cross_chain_request.target_chain_id()),
            Err(_) => None,
        }
    }
}