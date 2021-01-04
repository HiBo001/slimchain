use crate::{
    behavior::raft::message::NewBlockRequest,
    http::{
        client_rpc::TxHttpRequest,
        common::*,
        config::{NetworkRouteTable, PeerId},
        node_rpc::*,
    },
};
use async_raft::{
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
    NodeId, RaftNetwork,
};
use async_trait::async_trait;
use futures::{
    channel::{mpsc, oneshot},
    future,
    prelude::*,
};
use serde::{Deserialize, Serialize};
use slimchain_chain::{block_proposal::BlockProposal, consensus::raft::Block, role::Role};
use slimchain_common::{
    error::{bail, Result},
    tx::TxTrait,
};
use slimchain_tx_state::TxProposal;
use slimchain_utils::record_event;
use std::{marker::PhantomData, sync::Arc};
use tokio::task::JoinHandle;

pub struct ClientNodeNetwork<Tx>
where
    Tx: TxTrait + Serialize + for<'de> Deserialize<'de> + 'static,
{
    route_table: NetworkRouteTable,
    _marker: PhantomData<Tx>,
}

impl<Tx> ClientNodeNetwork<Tx>
where
    Tx: TxTrait + Serialize + for<'de> Deserialize<'de> + 'static,
{
    pub fn new(route_table: NetworkRouteTable) -> Self {
        Self {
            route_table,
            _marker: PhantomData,
        }
    }

    #[tracing::instrument(level = "debug", skip(self, tx_req))]
    pub async fn forward_tx_to_storage_node(&self, tx_req: TxHttpRequest) {
        let TxHttpRequest { req, shard_id } = tx_req;
        let tx_req_id = req.id();

        let storage_node_peer_id = match self.route_table.random_peer(&Role::Storage(shard_id)) {
            Some(peer) => peer,
            None => {
                error!(%tx_req_id , "Failed to find the storage node. ShardId: {:?}", shard_id);
                return;
            }
        };
        debug_assert_ne!(storage_node_peer_id, self.route_table.peer_id());

        let storage_node_addr = match self.route_table.peer_address(storage_node_peer_id) {
            Ok(addr) => addr,
            Err(_) => {
                error!(%tx_req_id , "Failed to get the storage address. PeerId: {}", storage_node_peer_id);
                return;
            }
        };

        record_event!("tx_begin", "tx_id": tx_req_id);

        let resp: Result<()> = send_post_request_using_postcard(
            &format!(
                "http://{}/{}/{}",
                storage_node_addr, NODE_RPC_ROUTE_PATH, STORAGE_TX_REQ_ROUTE_PATH
            ),
            &req,
        )
        .await;

        if let Err(e) = resp {
            error!(
                %tx_req_id,
                "Failed to forward TX to storage node. Error: {}", e
            );
        }
    }

    #[allow(clippy::ptr_arg)]
    #[allow(clippy::unit_arg)]
    #[tracing::instrument(level = "debug", skip(self, tx_proposals), err)]
    pub async fn forward_tx_proposal_to_leader(
        &self,
        leader: PeerId,
        tx_proposals: &Vec<TxProposal<Tx>>,
    ) -> Result<()> {
        debug_assert_ne!(leader, self.route_table.peer_id());
        let addr = self.route_table.peer_address(leader)?;
        send_reqs_to_leader(addr, tx_proposals).await
    }

    #[allow(clippy::unit_arg)]
    #[tracing::instrument(level = "debug", skip(self, block_proposal), fields(height = block_proposal.get_block_height().0), err)]
    pub async fn broadcast_block_proposal_to_storage_node(
        &self,
        block_proposal: &BlockProposal<Block, Tx>,
    ) -> Result<()> {
        let block_height = block_proposal.get_block_height();
        let bytes = bytes::Bytes::from(postcard::to_allocvec(block_proposal)?);
        let reqs = self
            .route_table
            .role_table()
            .iter()
            .filter(|(role, _)| matches!(role, Role::Storage(_)))
            .flat_map(|(_, list)| list.iter())
            .filter_map(|&peer_id| match self.route_table.peer_address(peer_id) {
                Ok(addr) => Some((
                    peer_id,
                    format!(
                        "http://{}/{}/{}",
                        addr, NODE_RPC_ROUTE_PATH, STORAGE_BLOCK_IMPORT_ROUTE_PATH
                    ),
                )),
                Err(_) => {
                    warn!("Failed to get the peer address. PeerId: {}", peer_id);
                    None
                }
            })
            .map(|(peer_id, uri)| {
                let bytes = bytes.clone();
                async move {
                    (
                        peer_id,
                        send_post_request_using_postcard_bytes::<()>(&uri, bytes).await,
                    )
                }
            });

        for (peer_id, resp) in future::join_all(reqs).await {
            if let Err(e) = resp {
                error!(%block_height, %peer_id, "Failed to broadcast block proposal to storage node. Err: {:?}", e);
            }
        }

        Ok(())
    }
}

#[async_trait]
impl<Tx> RaftNetwork<NewBlockRequest<Tx>> for ClientNodeNetwork<Tx>
where
    Tx: TxTrait + Serialize + for<'de> Deserialize<'de> + 'static,
{
    #[tracing::instrument(level = "debug", skip(self, rpc))]
    async fn append_entries(
        &self,
        target: NodeId,
        rpc: AppendEntriesRequest<NewBlockRequest<Tx>>,
    ) -> Result<AppendEntriesResponse> {
        let peer_id = PeerId::from(target);
        debug_assert_ne!(peer_id, self.route_table.peer_id());
        let addr = self.route_table.peer_address(peer_id)?;
        send_post_request_using_postcard(
            &format!(
                "http://{}/{}/{}",
                addr, NODE_RPC_ROUTE_PATH, RAFT_APPEND_ENTRIES_ROUTE_PATH
            ),
            &rpc,
        )
        .await
    }

    #[tracing::instrument(level = "debug", skip(self, rpc))]
    async fn install_snapshot(
        &self,
        target: NodeId,
        rpc: InstallSnapshotRequest,
    ) -> Result<InstallSnapshotResponse> {
        let peer_id = PeerId::from(target);
        debug_assert_ne!(peer_id, self.route_table.peer_id());
        let addr = self.route_table.peer_address(peer_id)?;
        send_post_request_using_postcard(
            &format!(
                "http://{}/{}/{}",
                addr, NODE_RPC_ROUTE_PATH, RAFT_INSTALL_SNAPSHOT_ROUTE_PATH
            ),
            &rpc,
        )
        .await
    }

    #[tracing::instrument(level = "debug", skip(self, rpc))]
    async fn vote(&self, target: NodeId, rpc: VoteRequest) -> Result<VoteResponse> {
        let peer_id = PeerId::from(target);
        debug_assert_ne!(peer_id, self.route_table.peer_id());
        let addr = self.route_table.peer_address(peer_id)?;
        send_post_request_using_postcard(
            &format!(
                "http://{}/{}/{}",
                addr, NODE_RPC_ROUTE_PATH, RAFT_VOTE_ROUTE_PATH
            ),
            &rpc,
        )
        .await
    }
}

pub struct ClientNodeNetworkWorker<Tx>
where
    Tx: TxTrait + Serialize + for<'de> Deserialize<'de> + 'static,
{
    handle: Option<JoinHandle<()>>,
    req_tx: mpsc::UnboundedSender<TxHttpRequest>,
    block_proposal_tx: mpsc::UnboundedSender<BlockProposal<Block, Tx>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl<Tx> ClientNodeNetworkWorker<Tx>
where
    Tx: TxTrait + Serialize + for<'de> Deserialize<'de> + 'static,
{
    pub fn new(network: Arc<ClientNodeNetwork<Tx>>) -> Self {
        let (req_tx, req_rx) = mpsc::unbounded();
        let mut req_rx = req_rx.fuse();
        let (block_proposal_tx, block_proposal_rx) = mpsc::unbounded();
        let mut block_proposal_rx = block_proposal_rx.fuse();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    req = req_rx.next() => {
                        if let Some(req) = req {
                            network.forward_tx_to_storage_node(req).await;
                        }
                    }
                    block_proposal = block_proposal_rx.next() => {
                        if let Some(block_proposal) = block_proposal {
                            network.broadcast_block_proposal_to_storage_node(&block_proposal).await.ok();
                        }
                    }
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        Self {
            handle: Some(handle),
            req_tx,
            block_proposal_tx,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    pub fn get_req_tx(&self) -> mpsc::UnboundedSender<TxHttpRequest> {
        self.req_tx.clone()
    }

    pub fn get_block_proposal_tx(&self) -> mpsc::UnboundedSender<BlockProposal<Block, Tx>> {
        self.block_proposal_tx.clone()
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.req_tx.close_channel();
        self.block_proposal_tx.close_channel();
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            shutdown_tx.send(()).ok();
        } else {
            bail!("Already shutdown.");
        }
        if let Some(handler) = self.handle.take() {
            handler.await?;
        } else {
            bail!("Already shutdown.");
        }
        Ok(())
    }
}
