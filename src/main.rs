mod config;
mod dag;
mod secure_wallet;
mod p2p_stitch;

use anyhow::Result;
use kaspa_wallet_core::tx::TransactionOutput;
use kaspa_addresses::Address;
use std::collections::HashSet;
use chrono::Utc;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cfg = config::Config::from_file("config.toml")?;

    let mut wallet = secure_wallet::load_or_create_wallet(&cfg.rpc_url.replace("ws", "http")).await?;
    let sk = wallet.private_key().clone();

    let mut rolling_dag = dag::RollingDag::new(cfg.dag_window);
    let rpc_http = kaspa_rpc_core::client::RpcClient::new(&cfg.rpc_url.replace("ws", "http"))?;
    let tips = rpc_http.get_tip_hashes().await?;
    for hash in tips.iter().rev().take(cfg.dag_window) {
        if let Ok(block) = rpc_http.get_block(hash).await {
            rolling_dag.add_block(block);
        }
    }
    log::info!("DAG bootstrapped: {} nodes", rolling_dag.graph.node_count());

    let p2p_adaptor = p2p_stitch::setup_p2p(&cfg).await?;
    let mut block_stream = kaspa_rpc_core::notifier::Notifier::new(rpc_http.clone()).await?.start().await?;

    let mut last_stitch = Utc::now().timestamp() - cfg.rate_limit_seconds as i64;

    while let Ok(notification) = block_stream.recv().await {
        if let kaspa_rpc_core::Notification::BlockAdded(block) = notification {
            log::info!("New block: {} (blue={})", block.hash(), block.header.blue_score);
            rolling_dag.add_block(block.clone());

            if Utc::now().timestamp() - last_stitch < cfg.rate_limit_seconds as i64 {
                continue;
            }

            if let Some((weak_idx, tips)) = rolling_dag.find_fracture(cfg.min_blue_delta) {
                let weak = &rolling_dag.graph[weak_idx];
                let tip_hashes: Vec<String> = tips.iter().map(|&i| rolling_dag.graph[i].hash.clone()).collect();
                log::info!("Fracture: {} | tips: {:?}", weak.hash, tip_hashes);

                p2p_stitch::broadcast_stitch(&p2p_adaptor, &weak.hash, &tip_hashes, cfg.stitch_reward_sompi, &sk).await?;
                log::info!("P2P stitch request sent");

                let tip_set: HashSet<String> = tip_hashes.into_iter().collect();
                let reward = cfg.stitch_reward_sompi;
                let wallet_clone = wallet.clone();
                tokio::spawn(async move {
                    for _ in 0..30 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        if let Ok(new_block) = rpc_http.get_block(&block.hash()).await {
                            let parents: HashSet<String> = new_block.header.direct_parents.iter().map(|h| h.to_string()).collect();
                            if tip_set.is_subset(&parents) {
                                if let Some(miner_addr) = get_miner_address(&new_block) {
                                    if let Ok(txid) = send_reward(&wallet_clone, miner_addr, reward).await {
                                        log::info!("HEALED! Reward sent: {}", txid);
                                    }
                                    return;
                                }
                            }
                        }
                    }
                });

                last_stitch = Utc::now().timestamp();
            }
        }
    }

    Ok(())
}

fn get_miner_address(block: &kaspa_consensus_core::block::Block) -> Option<Address> {
    block.transactions.first()?.outputs.first()?.script_public_key.address().ok()
}

async fn send_reward(wallet: &kaspa_wallet_core::wallet::Wallet<InMemoryStorage>, addr: Address, amount: u64) -> Result<String> {
    let mut tx = wallet.create_transaction(&addr, amount).await?;
    let rpc = kaspa_rpc_core::client::RpcClient::new("http://127.0.0.1:16110")?;
    Ok(rpc.submit_transaction(tx.into()).await?)
}
