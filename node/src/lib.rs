extern crate alloc;
extern crate core;

mod cryptography;
mod light_clients;
pub mod p2p;
pub mod rpc;
pub mod telemetry;
pub mod tx_processing;

use crate::p2p::P2pNetworkService;
use crate::rpc::{Airtable, TransactionRpcServer};
use alloc::sync::Arc;
use alloy::hex;
use anyhow::{anyhow, Error};
use codec::Decode;
use core::str::FromStr;
use db::db::saved_peers::Data;
use db::DbWorker;
use jsonrpsee::server::ServerBuilder;
use libp2p::futures::{FutureExt, StreamExt};
use libp2p::request_response::{InboundRequestId, Message, ResponseChannel};
use libp2p::{Multiaddr, PeerId};
use local_ip_address::local_ip;
use log::{error, info, warn};
use p2p::P2pWorker;
use primitives::data_structure::{
    new_tx_state_from_mutex, ChainSupported, DbTxStateMachine, NetworkCommand, PeerRecord,
    SwarmMessage, TxStateMachine, TxStatus,
};
use rand::Rng;
use rpc::TransactionRpcWorker;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::SocketAddr;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;
use tx_processing::TxProcessingWorker;

/// Main thread to be spawned by the application
/// this encompasses all node's logic and processing flow
#[derive(Clone)]
pub struct MainServiceWorker {
    pub db_worker: Arc<Mutex<DbWorker>>,
    pub tx_rpc_worker: Arc<Mutex<TransactionRpcWorker>>,
    pub tx_processing_worker: Arc<Mutex<TxProcessingWorker>>,
    pub airtable_client: Airtable,
    // for swarm events
    pub p2p_worker: Arc<Mutex<P2pWorker>>, //telemetry_worker: TelemetryWorker,
    pub p2p_network_service: Arc<Mutex<P2pNetworkService>>,
    // channels for layers communication
    /// sender channel to propagate transaction state to rpc layer
    pub rpc_sender_channel: Arc<Mutex<Sender<TxStateMachine>>>,
    /// receiver channel to handle the updates made by user from rpc
    pub user_rpc_update_recv_channel: Arc<Mutex<Receiver<Arc<Mutex<TxStateMachine>>>>>,
}

impl MainServiceWorker {
    pub(crate) async fn new() -> Result<Self, anyhow::Error> {
        // CHANNELS
        // ===================================================================================== //
        // for rpc messages back and forth propagation
        let (rpc_sender_channel, rpc_recv_channel) = tokio::sync::mpsc::channel(10);
        let (user_rpc_update_sender_channel, user_rpc_update_recv_channel) =
            tokio::sync::mpsc::channel(10);

        // for p2p network commands
        let (p2p_command_tx, p2p_command_recv) = tokio::sync::mpsc::channel::<NetworkCommand>(10);

        let port = rand::thread_rng().gen_range(0..=u16::MAX);

        // DATABASE WORKER (LOCAL AND REMOTE )
        // ===================================================================================== //
        let db_worker = Arc::new(Mutex::new(
            DbWorker::initialize_db_client("db/dev.db").await?,
        ));

        // fetch to the db, if not then set one
        let airtable_client = Airtable::new()
            .await
            .map_err(|err| anyhow!("failed to instantiate airtable client, caused by: {err}"))?;

        // PEER TO PEER NETWORKING WORKER
        // ===================================================================================== //

        let p2p_worker = P2pWorker::new(
            Arc::new(Mutex::new(airtable_client.clone())),
            db_worker.clone(),
            port,
            p2p_command_recv,
        )
        .await?;

        let p2p_network_service =
            P2pNetworkService::new(Arc::new(p2p_command_tx), p2p_worker.clone())?;

        // TRANSACTION RPC WORKER
        // ===================================================================================== //

        let txn_rpc_worker = TransactionRpcWorker::new(
            airtable_client.clone(),
            db_worker.clone(),
            Arc::new(Mutex::new(rpc_recv_channel)),
            Arc::new(Mutex::new(user_rpc_update_sender_channel)),
            port,
            p2p_worker.node_id,
        )
        .await?;

        // TRANSACTION PROCESSING LAYER
        // ===================================================================================== //

        let tx_processing_worker = TxProcessingWorker::new((
            ChainSupported::Bnb,
            ChainSupported::Ethereum,
            ChainSupported::Solana,
        ))
        .await?;
        // ===================================================================================== //

        Ok(Self {
            db_worker,
            tx_rpc_worker: Arc::new(Mutex::new(txn_rpc_worker)),
            tx_processing_worker: Arc::new(Mutex::new(tx_processing_worker)),
            airtable_client,
            p2p_worker: Arc::new(Mutex::new(p2p_worker)),
            p2p_network_service: Arc::new(Mutex::new(p2p_network_service)),
            rpc_sender_channel: Arc::new(Mutex::new(rpc_sender_channel)),
            user_rpc_update_recv_channel: Arc::new(Mutex::new(user_rpc_update_recv_channel)),
        })
    }

    /// handle swarm events; this includes
    /// 1. sender sending requests to receiver to attest ownership and correctness of the recv address
    /// 2. receiver response and sender handling submission of the tx
    pub(crate) async fn handle_swarm_event_messages(
        &self,
        p2p_worker: Arc<Mutex<P2pWorker>>,
        txn_processing_worker: Arc<Mutex<TxProcessingWorker>>,
    ) -> Result<(), Error> {
        let (sender_channel, mut recv_channel) = tokio::sync::mpsc::channel(256);

        // Start swarm first and keep it running infinitely
        tokio::spawn(async move {
            let res = p2p_worker.lock().await.start_swarm(sender_channel).await;
            if res.is_err() {
                error!("start swarm failed");
            }
        });

        // This loop should never end - it continuously processes messages
        loop {
            if let Some(swarm_msg_result) = recv_channel.recv().await {
                match swarm_msg_result {
                    Ok(swarm_msg) => match swarm_msg {
                        SwarmMessage::Request { data, inbound_id } => {
                            let mut decoded_req: TxStateMachine = Decode::decode(&mut &data[..])
                                .expect("failed to decode request body");

                            let inbound_req_id = {
                                let mut req_id_hash = DefaultHasher::default();
                                inbound_id.hash(&mut req_id_hash);
                                req_id_hash.finish()
                            };

                            decoded_req.inbound_req_id = Some(inbound_req_id);
                            // ===================================================================== //
                            // propagate transaction state to rpc layer for user updating (receiver updating)
                            self.rpc_sender_channel
                                .lock()
                                .await
                                .send(decoded_req.clone())
                                .await?;
                            info!(target: "MainServiceWorker","propagating txn msg as a request to rpc layer for user interaction: {decoded_req:?}");
                        }
                        SwarmMessage::Response { data, outbound_id } => {
                            let mut decoded_resp: TxStateMachine = Decode::decode(&mut &data[..])
                                .expect("failed to decode request body");

                            let outbound_req_id = {
                                let mut req_id_hash = DefaultHasher::default();
                                outbound_id.hash(&mut req_id_hash);
                                req_id_hash.finish()
                            };

                            decoded_resp.outbound_req_id = Some(outbound_req_id);
                            // ===================================================================== //
                            // handle error, by returning the tx status to the sender
                            if let Ok(_) = txn_processing_worker
                                .lock()
                                .await
                                .validate_receiver_sender_address(&decoded_resp,"Receiver")
                            {
                                decoded_resp.recv_confirmation_passed();
                                info!(target:"MainServiceWorker","receiver confirmation passed")
                            } else {
                                decoded_resp.recv_confirmation_failed();
                                info!(target:"MainServiceWorker","receiver confirmation passed");
                                // record failed txn in local db
                                let db_tx = DbTxStateMachine {
                                    tx_hash: vec![],
                                    amount: decoded_resp.amount,
                                    network: decoded_resp.network,
                                    success: false,
                                };
                                self.db_worker.lock().await.update_failed_tx(db_tx).await?;
                            }

                            // propagate transaction state to rpc layer for user updating ( this time sender verification)
                            self.rpc_sender_channel
                                .lock()
                                .await
                                .send(decoded_resp.clone())
                                .await?;

                            info!(target: "MainServiceWorker","propagating txn msg as a response to rpc layer for user interaction: {decoded_resp:?}");
                        }
                    },
                    Err(err) => {
                        info!("no new messages from swarm: {err}");
                        // Don't return error, just log and continue
                        continue;
                    }
                }
            }
        }
    }

    /// genesis state of initialized tx is being handled by the following stages
    /// 1. check if the receiver address peer id is saved in local db if not then search in remote db
    /// 2. getting the recv peer-id then dial the target peer-id (receiver)
    /// 3. send the tx-state-machine object to receiver target id via p2p swarm for receiver to sign and attest ownership and correctness of the address
    pub(crate) async fn handle_genesis_tx_state(
        &self,
        txn: Arc<Mutex<TxStateMachine>>,
    ) -> Result<(), anyhow::Error> {
        // dial to target peer id from tx receiver
        let target_id = {
            let tx = txn.lock().await;
            tx.receiver_address.clone()
        };
        // check if the acc is present in local db
        // First try local DB
        let target_peer_result = {
            // Release DB lock immediately after query
            self.db_worker
                .lock()
                .await
                .get_saved_user_peers(target_id.clone())
                .await
        };

        match target_peer_result {
            Ok(acc) => {
                info!(target:"MainServiceWorker","target peer found in local db");
                // dial the target
                let multi_addr = acc.multi_addr.parse::<Multiaddr>()?;
                let peer_id = PeerId::from_str(&acc.node_id)?;

                {
                    self.p2p_network_service
                        .lock()
                        .await
                        .dial_to_peer_id(multi_addr.clone(), &peer_id)
                        .await?;
                }
                // send the tx-state-machine to target peerId
                {
                    self.p2p_network_service
                        .lock()
                        .await
                        .send_request(txn.clone(), peer_id, multi_addr)
                        .await?;
                }
            }
            Err(_err) => {
                // fetch from remote db
                info!(target:"MainServiceWorker","target peer not found in local db, fetching from remote db");

                let acc_ids = self.airtable_client.list_all_peers().await?;

                let target_id_addr = {
                    let tx = txn.lock().await;
                    tx.receiver_address.clone()
                };

                if !acc_ids.is_empty() {
                    let result_peer = acc_ids.into_iter().find_map(|discovery| {
                        match discovery
                            .clone()
                            .account_ids
                            .into_iter()
                            .find(|addr| addr == &target_id_addr)
                        {
                            Some(_) => {
                                let peer_record: PeerRecord = discovery.clone().into();
                                Some((discovery.peer_id, discovery.multi_addr, peer_record))
                            }
                            None => None,
                        }
                    });

                    if result_peer.is_some() {
                        // dial the target
                        info!(target:"MainServiceWorker","target peer found in remote db: {result_peer:?} \n");
                        let multi_addr = result_peer
                            .clone()
                            .expect("failed to get multi addr")
                            .1
                            .unwrap()
                            .parse::<Multiaddr>()
                            .map_err(|err| {
                                anyhow!("failed to parse multi addr, caused by: {err}")
                            })?;
                        let peer_id = PeerId::from_str(
                            &*result_peer
                                .clone()
                                .expect("failed to parse peer id")
                                .0
                                .expect("failed to parse peerId"),
                        )?;

                        // save the target peer id to local db
                        let peer_record = result_peer.clone().unwrap().2;
                        info!(target: "MainServiceWorker","recording target peer id to local db");

                        // ========================================================================= //
                        {
                            self.db_worker
                                .lock()
                                .await
                                .record_saved_user_peers(peer_record)
                                .await?;
                        }

                        // ========================================================================= //
                        let mut p2p_network_service = self.p2p_network_service.lock().await;

                        {
                            p2p_network_service
                                .dial_to_peer_id(multi_addr.clone(), &peer_id)
                                .await?;
                        }

                        // wait for dialing to complete
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                        {
                            p2p_network_service
                                .send_request(txn.clone(), peer_id, multi_addr)
                                .await?;
                        }
                    } else {
                        error!(target: "MainServiceWorker","target peer not found in remote db,tell the user is missing out on safety transaction");
                    }
                }
            }
        }
        Ok(())
    }

    /// send the response to the sender via p2p swarm
    pub(crate) async fn handle_recv_addr_confirmed_tx_state(
        &self,
        id: u64,
        txn: Arc<Mutex<TxStateMachine>>,
    ) -> Result<(), anyhow::Error> {
        self.p2p_network_service
            .lock()
            .await
            .send_response(id, txn)
            .await?;
        Ok(())
    }

    /// last stage, submit the txn state-machine object to rpc to be signed and then submit to the target chain
    pub(crate) async fn handle_sender_confirmed_tx_state(
        &self,
        txn: Arc<Mutex<TxStateMachine>>,
    ) -> Result<(), Error> {
        let txn_guard = txn.lock().await;
        let mut txn_inner = new_tx_state_from_mutex(txn_guard);

        if let Some(_) = txn_inner.signed_call_payload {
            // verify first sender confirmation
            if let Ok(_) = self.tx_processing_worker.lock().await.validate_receiver_sender_address(&txn_inner,"Sender"){
                // TODO! handle submission errors
                // signed and ready to be submitted to target chain
                match self
                    .tx_processing_worker
                    .lock()
                    .await
                    .submit_tx(txn_inner.clone())
                    .await
                {
                    Ok(tx_hash) => {
                        // update user via rpc on tx success
                        txn_inner.tx_submission_passed(tx_hash);
                        self.rpc_sender_channel
                            .lock()
                            .await
                            .send(txn_inner.clone())
                            .await?;
                        // update local db on success tx
                        let db_tx = DbTxStateMachine {
                            tx_hash: tx_hash.to_vec(),
                            amount: txn_inner.amount.clone(),
                            network: txn_inner.network.clone(),
                            success: true,
                        };
                        self.db_worker.lock().await.update_success_tx(db_tx).await?;
                    }
                    Err(err) => {
                        txn_inner.tx_submission_failed(format!(
                            "{err:?}: the tx will be resubmitted rest assured"
                        ));
                        self.rpc_sender_channel.lock().await.send(txn_inner).await?;
                    }
                }

            }else{
                // non original sender confirmed, return error, send to rpc
                txn_inner.sender_confirmation_failed();
                error!(target: "MainServiceWorker","Non original sender signed");
                self.rpc_sender_channel.lock().await.send(txn_inner).await?;

            }

        } else {
            // verify multi-id, attesting to the actual recv address intended to send
            if !self
                .tx_processing_worker
                .lock()
                .await
                .validate_multi_id(&txn_inner)
            {
                // if not true then send the txn state to sender and record to local db as failed tx
                // and this is added to amount saved from loss as the sender could have sent to a wrong address

                let db_txn = DbTxStateMachine {
                    tx_hash: vec![],
                    amount: txn_inner.amount,
                    network: txn_inner.network,
                    success: false,
                };

                self.db_worker.lock().await.update_failed_tx(db_txn).await?;
                error!(target:"MainServiceWorker","multi-id verification failed, hence receiver is not one intended");
                self.rpc_sender_channel.lock().await.send(txn_inner).await?;

            } else {
                // ===================================================================== //
                // not signed yet, send to rpc to be signed
                let to_send_tx_state_machine = self
                    .tx_processing_worker
                    .lock()
                    .await
                    .create_tx(txn_inner)
                    .await?;

                self.rpc_sender_channel
                    .lock()
                    .await
                    .send(to_send_tx_state_machine)
                    .await?;
            }
        }

        Ok(())
    }

    /// this for now is same as `handle_addr_confirmed_tx_state`
    pub(crate) async fn handle_net_confirmed_tx_state(
        &self,
        _txn: Arc<Mutex<TxStateMachine>>,
    ) -> Result<(), anyhow::Error> {
        todo!()
    }

    /// all user interactions are done via rpc, after user sends rpc as updated (`tx-state-machine`) as argument,
    /// the tx object will be send to channel to be handled depending on its current state
    pub(crate) async fn handle_incoming_rpc_tx_updates(&self) -> Result<(), anyhow::Error> {
        while let Some(txn) = self.user_rpc_update_recv_channel.lock().await.recv().await {
            // handle the incoming transaction per its state
            let status = txn.lock().await.clone().status;
            match status {
                TxStatus::Genesis => {
                    info!(target:"MainServiceWorker","handling incoming genesis tx updates: {:?} \n",txn.lock().await.clone());
                    self.handle_genesis_tx_state(txn.clone()).await?;
                }

                TxStatus::RecvAddrConfirmed => {
                    info!(target:"MainServiceWorker","handling incoming receiver addr-confirmation tx updates: {:?} \n",txn.lock().await.clone());

                    let inbound_id = txn
                        .lock()
                        .await
                        .inbound_req_id
                        .expect("no inbound req id found");
                    self.handle_recv_addr_confirmed_tx_state(inbound_id, txn.clone())
                        .await?;
                }

                TxStatus::NetConfirmed => {
                    todo!()
                }

                TxStatus::SenderConfirmed => {
                    info!(target:"MainServiceWorker","handling incoming sender addr-confirmed tx updates: {:?} \n",txn.lock().await.clone());

                    self.handle_sender_confirmed_tx_state(txn.clone()).await?;
                }
                _ => {}
            };
        }
        Ok(())
    }

    /// Start rpc server with default url
    pub(crate) async fn start_rpc_server(&self) -> Result<SocketAddr, anyhow::Error> {
        let server_builder = ServerBuilder::new();

        let url = self.tx_rpc_worker.lock().await.rpc_url.clone();
        let rpc_handler = self.tx_rpc_worker.clone().lock().await.clone();

        let server = server_builder.build(url).await?;
        let address = server
            .local_addr()
            .map_err(|err| anyhow!("failed to get address: {}", err))?;
        let handle = server
            .start(rpc_handler.into_rpc())
            .map_err(|err| anyhow!("rpc handler error: {}", err))?;

        tokio::spawn(handle.stopped());
        Ok(address)
    }

    /// compose all workers and run logically, the p2p swarm worker will be running indefinately on background same as rpc worker
    pub async fn run() -> Result<(), anyhow::Error> {
        info!(
            "\n🔥 =========== Vane Web3 =========== 🔥\n\
             A safety layer for web3 transactions, allows you to feel secure when sending and receiving \n\
             tokens without the fear of selecting the wrong address or network. \n\
             It provides a safety net, giving you room to make mistakes without losing all your funds.\n"
        );

        // ====================================================================================== //
        let main_worker = Self::new().await?;
        // start rpc server
        let rpc_address = main_worker
            .start_rpc_server()
            .await
            .map_err(|err| anyhow!("failed to start rpc server, caused by: {err}"))?;

        info!(target: "RpcServer","listening to rpc url: {rpc_address}");
        // ====================================================================================== //

        let p2p_worker = main_worker.p2p_worker.clone();
        let txn_processing_worker = main_worker.tx_processing_worker.clone();

        // ====================================================================================== //

        let tokio_handle = tokio::runtime::Handle::current();
        let mut task_manager = sc_service::TaskManager::new(tokio_handle, None)?;

        // ====================================================================================== //

        {
            let cloned_main_worker = main_worker.clone();
            let task_name = "transaction-handling-task".to_string();
            task_manager.spawn_essential_handle().spawn_blocking(
                Box::leak(Box::new(task_name)),
                "transaction-handling",
                async move {
                    // watch tx messages from tx rpc worker and pass it to p2p to be verified by receiver
                    let res = cloned_main_worker.handle_incoming_rpc_tx_updates().await;
                    if let Err(err) = res {
                        error!("rpc handle encountered error: caused by {err}");
                    }
                }
                .boxed(),
            )
        }

        {
            let task_name = "swarm-p2p-task".to_string();
            task_manager.spawn_essential_handle().spawn_blocking(
                Box::leak(Box::new(task_name)),
                "swarm",
                async move {
                    let res = main_worker
                        .handle_swarm_event_messages(p2p_worker, txn_processing_worker)
                        .await;
                    if let Err(err) = res {
                        error!("swarm handle encountered error; caused by {err}");
                    }
                }
                .boxed(),
            )
        }

        task_manager.future().await?;

        Ok(())
    }

    // =================================== E2E ====================================== //

    #[cfg(feature = "e2e")]
    pub async fn e2e_new(port: u16, db: &str) -> Result<Self, Error> {
        // CHANNELS
        // ===================================================================================== //
        // for rpc messages back and forth propagation
        let (rpc_sender_channel, rpc_recv_channel) = tokio::sync::mpsc::channel(10);
        let (user_rpc_update_sender_channel, user_rpc_update_recv_channel) =
            tokio::sync::mpsc::channel(10);

        // for p2p network commands
        let (p2p_command_tx, p2p_command_recv) = tokio::sync::mpsc::channel::<NetworkCommand>(10);

        // DATABASE WORKER (LOCAL AND REMOTE )
        // ===================================================================================== //
        let db_worker = Arc::new(Mutex::new(DbWorker::initialize_db_client(db).await?));

        // fetch to the db, if not then set one
        let airtable_client = Airtable::new()
            .await
            .map_err(|err| anyhow!("failed to instantiate airtable client, caused by: {err}"))?;

        // PEER TO PEER NETWORKING WORKER
        // ===================================================================================== //

        let p2p_worker = P2pWorker::new(
            Arc::new(Mutex::new(airtable_client.clone())),
            db_worker.clone(),
            port,
            p2p_command_recv,
        )
        .await?;

        let p2p_network_service =
            P2pNetworkService::new(Arc::new(p2p_command_tx), p2p_worker.clone())?;

        // TRANSACTION RPC WORKER
        // ===================================================================================== //

        let txn_rpc_worker = TransactionRpcWorker::new(
            airtable_client.clone(),
            db_worker.clone(),
            Arc::new(Mutex::new(rpc_recv_channel)),
            Arc::new(Mutex::new(user_rpc_update_sender_channel)),
            port,
            p2p_worker.node_id,
        )
        .await?;

        // TRANSACTION PROCESSING LAYER
        // ===================================================================================== //

        let tx_processing_worker = TxProcessingWorker::new((
            ChainSupported::Bnb,
            ChainSupported::Ethereum,
            ChainSupported::Solana,
        ))
        .await?;
        // ===================================================================================== //

        Ok(Self {
            db_worker,
            tx_rpc_worker: Arc::new(Mutex::new(txn_rpc_worker)),
            tx_processing_worker: Arc::new(Mutex::new(tx_processing_worker)),
            airtable_client,
            p2p_worker: Arc::new(Mutex::new(p2p_worker)),
            p2p_network_service: Arc::new(Mutex::new(p2p_network_service)),
            rpc_sender_channel: Arc::new(Mutex::new(rpc_sender_channel)),
            user_rpc_update_recv_channel: Arc::new(Mutex::new(user_rpc_update_recv_channel)),
        })
    }

    #[cfg(feature = "e2e")]
    pub async fn e2e_run(main_worker: MainServiceWorker) -> Result<(), anyhow::Error> {
        // ====================================================================================== //
        // start rpc server
        let rpc_address = main_worker
            .start_rpc_server()
            .await
            .map_err(|err| anyhow!("failed to start rpc server, caused by: {err}"))?;

        info!(target: "RpcServer","listening to rpc url: {rpc_address}");
        // ====================================================================================== //

        let p2p_worker = main_worker.p2p_worker.clone();
        let txn_processing_worker = main_worker.tx_processing_worker.clone();

        // ====================================================================================== //

        let tokio_handle = tokio::runtime::Handle::current();
        let mut task_manager = sc_service::TaskManager::new(tokio_handle, None)?;

        // ====================================================================================== //

        {
            let cloned_main_worker = main_worker.clone();
            let task_name = "transaction-handling-task".to_string();
            task_manager.spawn_essential_handle().spawn_blocking(
                Box::leak(Box::new(task_name)),
                "transaction-handling",
                async move {
                    // watch tx messages from tx rpc worker and pass it to p2p to be verified by receiver
                    let res = cloned_main_worker.handle_incoming_rpc_tx_updates().await;
                    if let Err(err) = res {
                        error!("rpc handle encountered error: caused by {err}");
                    }
                }
                .boxed(),
            )
        }

        {
            let task_name = "swarm-p2p-task".to_string();
            task_manager.spawn_essential_handle().spawn_blocking(
                Box::leak(Box::new(task_name)),
                "swarm",
                async move {
                    let res = main_worker
                        .handle_swarm_event_messages(p2p_worker, txn_processing_worker)
                        .await;
                    if let Err(err) = res {
                        error!("swarm handle encountered error; caused by {err}");
                    }
                }
                .boxed(),
            )
        }

        task_manager.future().await?;

        Ok(())
    }
}
