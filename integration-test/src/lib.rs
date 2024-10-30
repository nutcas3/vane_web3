use codec::{Decode, Encode};
use db::DbWorker;
use libp2p::{Multiaddr, PeerId};
use node::p2p::{BoxStream, P2pWorker};
use node::MainServiceWorker;
use primitives::data_structure::{ChainSupported, PeerRecord};
use simplelog::*;
use std::fs::File;
use std::sync::Arc;
use tokio::sync::Mutex;

fn log_setup() -> Result<(), anyhow::Error> {
    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Info,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(
            LevelFilter::Info,
            Config::default(),
            File::create("vane-test.log").unwrap(),
        ),
    ])?;
    Ok(())
}

#[cfg(feature = "e2e")]
mod e2e_tests {
    use super::*;
    use crate::log_setup;
    use anyhow::{anyhow, Error};
    use db::db::new_client_with_url;
    use libp2p::futures::StreamExt;
    use libp2p::request_response::Message;
    use log::{error, info};
    use node::MainServiceWorker;
    use primitives::data_structure::{SwarmMessage, TxStateMachine};
    use std::sync::Arc;

    // having 2 peers; peer 1 sends a tx-state-machine message to peer 2
    // and peer2 respond a modified version of tx-state-machine.
    // and the vice-versa
    #[tokio::test]
    async fn p2p_test() -> Result<(), anyhow::Error> {
        log_setup();

        let main_worker_1 = MainServiceWorker::e2e_new(3000, "../db/test1.db").await?;
        let main_worker_2 = MainServiceWorker::e2e_new(4000, "../db/test2.db").await?;

        // Test state structure
        struct TestState {
            sent_msg: TxStateMachine,
            response_msg: TxStateMachine,
        }

        // Create shared state using Arc
        let test_state = Arc::new(TestState {
            sent_msg: TxStateMachine::default(),
            response_msg: TxStateMachine {
                amount: 1000,
                ..Default::default()
            },
        });

        // Spawn worker 1 task
        let worker_1 = main_worker_1.clone();
        let state_1 = test_state.clone();

        let swarm_task_1 = tokio::spawn(async move {
            let mut swarm = worker_1.p2p_worker.lock().await.start_swarm().await?;

            while let Some(event) = swarm.next().await {
                match event {
                    Ok(SwarmMessage::Request { .. }) => {
                        info!("Worker 1 received request");
                    }
                    Ok(SwarmMessage::Response { data, outbound_id }) => {
                        let received_response: TxStateMachine =
                            Decode::decode(&mut &data[..]).unwrap();
                        assert_eq!(received_response, state_1.response_msg);
                        return Ok(());
                    }
                    Err(e) => error!("Worker 1 error: {}", e),
                }
            }
            Ok::<_, anyhow::Error>(())
        });

        // Spawn worker 2 task
        let worker_2 = main_worker_2.clone();
        let state_2 = test_state.clone();

        let swarm_task_2 = tokio::spawn(async move {
            let mut swarm = worker_2.p2p_worker.lock().await.start_swarm().await?;

            while let Some(event) = swarm.next().await {
                match event {
                    Ok(SwarmMessage::Request { data, inbound_id }) => {
                        println!("received a req: {data:?}");
                        // worker_2
                        //     .req_resp
                        //     .lock()
                        //     .await
                        //     .send_response(
                        //         channel,
                        //         Arc::new(Mutex::new(state_2.response_msg.clone())),
                        //     )
                        //     .await?;
                    }
                    Ok(SwarmMessage::Response { .. }) => {
                        info!("Worker 2 received response");
                    }
                    Err(e) => error!("Worker 2 error: {}", e),
                }
            }
            Ok::<_, anyhow::Error>(())
        });

        // sending the request
        let peer_id_2 = main_worker_2.p2p_worker.lock().await.node_id;
        let multi_addr_2 = main_worker_2.p2p_worker.lock().await.url.clone();

        main_worker_1
            .p2p_worker
            .lock()
            .await
            .dial_to_peer_id(multi_addr_2, peer_id_2)
            .await?;

        main_worker_1
            .p2p_worker
            .lock()
            .await
            .send_request(Arc::new(Mutex::new(test_state.sent_msg.clone())), peer_id_2)
            .await?;

        swarm_task_1.await??;
        swarm_task_2.await??;

        Ok(())
    }

    #[tokio::test]
    async fn rpc_test() -> Result<(), anyhow::Error> {
        log_setup();
        // // test airtable data
        // let rpc_worker = RpcWorker::new().await?;
        // let data = rpc_worker
        //     .airtable_client
        //     .lock()
        //     .await
        //     .list_all_peers()
        //     .await?;

        Ok(())
    }

    #[tokio::test]
    async fn airtable_test() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn transaction_processing_test() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn telemetry_test() -> Result<(), anyhow::Error> {
        Ok(())
    }

    // user creating an account, and sending a correct eth transaction works with recv and sender confirmation
    #[tokio::test]
    async fn user_flow_eth_works() -> Result<(), anyhow::Error> {
        Ok(())
    }

    // user creating an account, and sending a wrong eth address transaction reverts
    #[tokio::test]
    async fn user_flow_eth_wrong_address_reverts() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn user_flow_erc20_works() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn user_flow_bnb_works() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn user_flow_bnb_wrong_network_reverts() -> Result<(), anyhow::Error> {
        Ok(())
    }
    #[tokio::test]
    async fn user_flow_bnb_reverts() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn user_flow_brc20_works() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn revenue_eth_works() -> Result<(), anyhow::Error> {
        Ok(())
    }

    #[tokio::test]
    async fn revenue_bnb_works() -> Result<(), anyhow::Error> {
        Ok(())
    }
}
