mod config;
mod dag;
mod secure_wallet;
mod p2p_stitch;

use anyhow::{Result, Context};
use kaspa_wallet_core::tx::TransactionOutput;
use kaspa_addresses::Address;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use chrono::Utc;
use tokio::signal;
use log::{info, warn, error};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    info!("Stitchbot starting up...");
    
    // Allow graceful shutdown
    let shutdown = async {
        signal::ctrl_c().await.expect("Failed to install CTRL+C handler");
        warn!("Received shutdown signal (CTRL+C)");
    };

    let runner = async {
        let cfg = config::Config::from_file("config.toml")
            .context("Failed to load config.toml")?;

        // Protect wallet with Arc<Mutex<>>
        let wallet = Arc::new(Mutex::new(
            secure_wallet::load_or_create_wallet(&cfg.rpc_url.replace("ws", "http")).await
                .context("Failed to init wallet")?
        ));
        let sk = wallet.lock().await.private_key().clone();

        // Use configured HTTP endpoint everywhere
        let rpc_http_url = cfg.rpc_url.replace("ws", "http");
        let mut rolling_dag = dag::RollingDag::new(cfg.dag_window);
        let rpc_http = kaspa_rpc_core::client::RpcClient::new(&rpc_http_url)
            .context("Failed to create Kaspa RPC client")?;
        let tips = rpc_http.get_tip_hashes().await
            .context("Failed to fetch tip hashes")?;
        for hash in tips.iter().rev().take(cfg.dag_window) {
            match rpc_http.get_block(hash).await {
                Ok(block) => rolling_dag.add_block(block),
                Err(e) => warn!("Failed to fetch block {}: {}", hash, e),
            }
        }
        info!("DAG bootstrapped: {} nodes", rolling_dag.graph.node_count());

        let p2p_adaptor = p2p_stitch::setup_p2p(&cfg).await
            .context("P2P setup failed")?;

        let mut block_stream = kaspa_rpc_core::notifier::Notifier::new(rpc_http.clone()).await
            .context("Failed to start block stream")?.start().await
            .context("Failed to start block notifications")?;

        let mut last_stitch = Utc::now().timestamp() - cfg.rate_limit_seconds as i64;

        loop {
            tokio::select! {
                maybe_notification = block_stream.recv() => {
                    let notification = match maybe_notification {
                        Ok(n) => n,
                        Err(e) => {
                            error!("Block stream error: {}", e);
                            break; // Stop on stream error
                        }
                    };
                    if let kaspa_rpc_core::Notification::BlockAdded(block) = notification {
                        info!("New block: {} (blue={})", block.hash(), block.header.blue_score);
                        rolling_dag.add_block(block.clone());

                        if Utc::now().timestamp() - last_stitch < cfg.rate_limit_seconds as i64 {
                            continue;
                        }

                        if let Some((weak_idx, tips)) = rolling_dag.find_fracture(cfg.min_blue_delta) {
                            let weak = &rolling_dag.graph[weak_idx];
                            let tip_hashes: Vec<String> = tips.iter().map(|&i| rolling_dag.graph[i].hash.clone()).collect();
                            info!("Fracture detected: {} | tips: {:?}", weak.hash, tip_hashes);

                            if let Err(e) = p2p_stitch::broadcast_stitch(
                                &p2p_adaptor,
                                &weak.hash,
                                &tip_hashes,
                                cfg.stitch_reward_sompi,
                                &sk
                            ).await {
                                error!("P2P stitch broadcast failed: {}", e);
                                continue;
                            }
                            info!("P2P stitch request sent");

                            let tip_set: HashSet<String> = tip_hashes.iter().cloned().collect();
                            let reward = cfg.stitch_reward_sompi;
                            let wallet_clone = wallet.clone();
                            let rpc_http_clone = rpc_http.clone();
                            let block_hash = block.hash().clone();

                            tokio::spawn(async move {
                                for attempt in 0..30 {
                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                    let new_block = match rpc_http_clone.get_block(&block_hash).await {
                                        Ok(b) => b,
                                        Err(e) => {
                                            warn!("Failed to fetch stitched block: {}", e);
                                            continue;
                                        }
                                    };
                                    let parents: HashSet<String> = new_block.header.direct_parents.iter().map(|h| h.to_string()).collect();
                                    if tip_set.is_subset(&parents) {
                                        match get_miner_address(&new_block) {
                                            Some(miner_addr) => {
                                                let txid = match send_reward(
                                                    &wallet_clone, miner_addr, reward, &rpc_http_url
                                                ).await {
                                                    Ok(id) => id,
                                                    Err(e) => {
                                                        error!("Reward send failed: {}", e);
                                                        continue;
                                                    }
                                                };
                                                info!("HEALED! Reward sent: {}", txid);
                                            }
                                            None => error!("Failed to determine miner address for reward payout"),
                                        }
                                        return;
                                    }
                                }
                                warn!("Reward for stitch healing not sent after 30 attempts");
                            });

                            last_stitch = Utc::now().timestamp();
                        }
                    }
                }
                _ = shutdown => {
                    info!("Shutting down stitchbot gracefully.");
                    break;
                }
            }
        }
        Ok(())
    };

    runner.await
}

fn get_miner_address(block: &kaspa_consensus_core::block::Block) -> Option<Address> {
    block.transactions
        .first()
        .and_then(|tx| tx.outputs.first())
        .and_then(|output| output.script_public_key.address().ok())
}

async fn send_reward(
    wallet: &Arc<Mutex<kaspa_wallet_core::wallet::Wallet<InMemoryStorage>>>,
    addr: Address,
    amount: u64,
    rpc_url: &str,
) -> Result<String> {
    let mut wallet_guard = wallet.lock().await;
    let tx = wallet_guard.create_transaction(&addr, amount).await
        .context("Failed to create reward transaction")?;
    let rpc = kaspa_rpc_core::client::RpcClient::new(rpc_url)
        .context("Failed to create RPC client for reward transaction")?;
    Ok(rpc.submit_transaction(tx.into()).await
        .context("Failed to submit reward transaction")?)
}
