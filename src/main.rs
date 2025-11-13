mod config;
mod dag;
mod secure_wallet;
mod p2p_stitch;
mod adaptive;

use anyhow::Result;
use kaspa_wallet_core::tx::TransactionOutput;
use kaspa_addresses::Address;
use std::collections::{HashSet, VecDeque};
use chrono::Utc;
use log::info;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cfg = config::Config::from_file("config.toml")?;

    let mut wallet = secure_wallet::load_or_create_wallet(&cfg.rpc_url.replace("ws", "http")).await?;
    let sk = wallet.private_key().clone();

    let mut rolling_dag = dag::RollingDag::new(10000);
    let rpc_http = kaspa_rpc_core::client::RpcClient::new(&cfg.rpc_url.replace("ws", "http"))?;
    let tips = rpc_http.get_tip_hashes().await?;
    for hash in tips.iter().rev().take(10000) {
        if let Ok(block) = rpc_http.get_block(hash).await {
            rolling_dag.add_block(block);
        }
    }
    info!("DAG bootstrapped: {} nodes", rolling_dag.graph.node_count());

    let p2p_adaptor = p2p_stitch::setup_p2p(&cfg).await?;
    let mut block_stream = kaspa_rpc_core::notifier::Notifier::new(rpc_http.clone()).await?.start().await?;

    // === ADAPTIVE ENGINE ===
    let mut adaptive_engine = cfg.adaptive.then(|| adaptive::AdaptiveEngine::new(cfg.clone()));

    // For BPS estimation
    let mut block_times = VecDeque::with_capacity(100);

    while let Ok(notification) = block_stream.recv().await {
        if let kaspa_rpc_core::Notification::BlockAdded(block) = notification {
            let now = Utc::now().timestamp_millis();
            block_times.push_back(now);
            if block_times.len() > 100 {
                block_times.pop_front();
            }
            let bps = if block_times.len() > 1 {
                let dt = (block_times.back().unwrap() - block_times.front().unwrap()) as f64 / 1000.0;
                (block_times.len() - 1) as f64 / dt.max(1.0)
            } else {
                1.0
            };

            info!("New block: {} (blue={}) | BPS: {:.1}", block.hash(), block.header.blue_score, bps);
            rolling_dag.add_block(block.clone());

            // Update adaptive engine
            if let Some(engine) = adaptive_engine.as_mut() {
                let is_orphan = false; // In real use, detect via RPC or DAG
                engine.update_block(&block, is_orphan);
            }

            if let Some((weak_idx, tips)) = rolling_dag.find_fracture(200) {
                let weak = &rolling_dag.graph[weak_idx];
                let tip_hashes: Vec<String> = tips.iter().map(|&i| rolling_dag.graph[i].hash.clone()).collect();
                let blue_delta = tips.iter()
                    .map(|&i| rolling_dag.graph[i].blue_score.saturating_sub(weak.blue_score))
                    .max()
                    .unwrap_or(0);

                let sus = adaptive_engine.as_ref()
                    .map(|e| e.sus(blue_delta, bps))
                    .unwrap_or(1.0);

                let should_stitch = adaptive_engine.as_ref()
                    .map(|e| e.should_stitch(blue_delta, bps))
                    .unwrap_or(true);

                let reward = adaptive_engine.as_ref()
                    .map(|e| e.reward(sus))
                    .unwrap_or(cfg.stitch_reward_sompi);

                let rate_limit = adaptive_engine.as_ref()
                    .map(|e| {
                        let base = cfg.base_rate_limit as f64;
                        let sus = sus.min(10.0);
                        (base / (1.0 + sus)) as i64
                    })
                    .unwrap_or(30);

                info!(
                    "Fracture: {} | delta={} | SUS={:.2} | rate={}s | reward={} sompi | stitch={}",
                    weak.hash, blue_delta, sus, rate_limit, reward, should_stitch
                );

                if should_stitch {
                    p2p_stitch::broadcast_stitch(&p2p_adaptor, &weak.hash, &tip_hashes, reward, &sk).await?;
                    info!("ADAPTIVE STITCH → reward={} sompi", reward);

                    let tip_set: HashSet<String> = tip_hashes.into_iter().collect();
                    let wallet_clone = wallet.clone();
                    let rpc_clone = rpc_http.clone();
                    tokio::spawn(async move {
                        for _ in 0..30 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            if let Ok(new_block) = rpc_clone.get_block(&block.hash()).await {
                                let parents: HashSet<String> = new_block.header.direct_parents.iter().map(|h| h.to_string()).collect();
                                if tip_set.is_subset(&parents) {
                                    if let Some(miner_addr) = get_miner_address(&new_block) {
                                        if let Ok(txid) = send_reward(&wallet_clone, miner_addr, reward).await {
                                            info!("HEALED! Reward sent: {}", txid);
                                        }
                                        return;
                                    }
                                }
                            }
                        }
                    });

                    if let Some(engine) = adaptive_engine.as_mut() {
                        engine.record_stitch();
                    }
                }
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
}
