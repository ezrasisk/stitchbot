mod config;
mod dag;
mod secure_wallet;
mod p2p_stitch;
mod adaptive;

use anyhow::Result;
use kaspa_addresses::Address;
use std::collections::{HashSet, VecDeque};
use chrono::Utc;
use log::{info, warn};

/// Interval for healing check in seconds.
const HEAL_CHECK_INTERVAL_SECS: u64 = 2;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cfg = config::Config::from_file("config.toml")?;

    // Avoid redundant string replacements by caching the HTTP RPC URL
    let rpc_http_url = cfg.rpc_url.replace("ws", "http");
    let mut wallet = secure_wallet::load_or_create_wallet(&rpc_http_url).await?;
    let sk = wallet.private_key().clone();

    let mut rolling_dag = dag::RollingDag::new(cfg.dag_window);
    let rpc_http = kaspa_rpc_core::client::RpcClient::new(&rpc_http_url)?;
    let tips = rpc_http.get_tip_hashes().await?;
    for hash in tips.iter().rev().take(cfg.dag_window) {
        if let Ok(block) = rpc_http.get_block(hash).await {
            rolling_dag.add_block(block);
        }
    }
    info!("DAG ready: {} blocks", rolling_dag.graph.node_count());

    let p2p_adaptor = p2p_stitch::setup_p2p(&cfg).await?;
    let mut block_stream = kaspa_rpc_core::notifier::Notifier::new(rpc_http.clone()).await?.start().await?;

    let mut adaptive_engine = cfg.adaptive.then(|| adaptive::AdaptiveEngine::new(cfg.clone()));
    let mut block_times = VecDeque::with_capacity(100);

    while let Ok(notification) = block_stream.recv().await {
        if let kaspa_rpc_core::Notification::BlockAdded(block) = notification {
            let now_ms = Utc::now().timestamp_millis();
            block_times.push_back(now_ms);
            if block_times.len() > 100 { block_times.pop_front(); }

            let bps = if block_times.len() > 1 {
                let dt = (block_times.back().unwrap() - block_times.front().unwrap()) as f64 / 1000.0;
                // Avoid division by zero, but do not distort metric if dt > 0 but < 1 ms
                if dt.abs() < f64::EPSILON {
                    0.0
                } else {
                    (block_times.len() - 1) as f64 / dt
                }
            } else { 1.0 };

            info!("Block: {} (blue={}) | BPS: {:.1}", block.hash(), block.header.blue_score, bps);
            rolling_dag.add_block(block.clone());

            let is_orphan = !rolling_dag.is_in_selected_chain(&block);
            if is_orphan { info!("ORPHAN: {}", block.hash()); }

            if let Some(engine) = adaptive_engine.as_mut() {
                engine.update_block(&block, is_orphan).await?;
            }

            let now = Utc::now().timestamp();

            if let Some((weak_idx, tips)) = rolling_dag.find_fracture(200) {
                let weak = &rolling_dag.graph[weak_idx];
                let tip_hashes: Vec<String> = tips.iter().map(|&i| rolling_dag.graph[i].hash.clone()).collect();
                let blue_delta = tips.iter()
                    .map(|&i| rolling_dag.graph[i].blue_score.saturating_sub(weak.blue_score))
                    .max()
                    .unwrap_or(0);

                let sus = adaptive_engine.as_ref().map(|e| e.sus(blue_delta, bps)).unwrap_or(1.0);
                let should_stitch = adaptive_engine.as_ref().map(|e| e.should_stitch(blue_delta, bps, now)).unwrap_or(true);
                let reward = adaptive_engine.as_ref().map(|e| e.reward(sus)).unwrap_or(cfg.base_reward_sompi);

                info!(
                    "Fracture: {} | delta={} | SUS={:.2} | reward={} | stitch={} | orphan_rate={:.3}%",
                    weak.hash, blue_delta, sus, reward, should_stitch,
                    adaptive_engine.as_ref().map(|e| e.orphan_rate() * 100.0).unwrap_or(0.0)
                );

                if should_stitch {
                    p2p_stitch::broadcast_stitch(&p2p_adaptor, &weak.hash, &tip_hashes, reward, &sk).await?;
                    info!("STITCHED → {} sompi", reward);

                    let tip_set: HashSet<String> = tip_hashes.iter().cloned().collect();
                    let wallet_clone = wallet.clone();
                    let rpc_clone = rpc_http.clone();
                    let block_hash = block.hash();
                    tokio::spawn(async move {
                        for _ in 0..30 {
                            tokio::time::sleep(std::time::Duration::from_secs(HEAL_CHECK_INTERVAL_SECS)).await;
                            if let Ok(new_block) = rpc_clone.get_block(&block_hash).await {
                                let parents: HashSet<String> = new_block.header.direct_parents.iter().map(|h| h.to_string()).collect();
                                if tip_set.is_subset(&parents) {
                                    if let Some(addr) = get_miner_address(&new_block) {
                                        match send_reward(&wallet_clone, addr, reward).await {
                                            Ok(txid) => info!("HEALED: {}", txid),
                                            Err(e) => warn!("Failed to send reward: {:?}", e),
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

/// Attempts to extract the miner address from the first transaction's first output of a block.
fn get_miner_address(block: &kaspa_consensus_core::block::Block) -> Option<Address> {
    block.transactions.first()
        .and_then(|tx| tx.outputs.first())
        .and_then(|out| out.script_public_key.address().ok())
}

/// Sends a reward transaction using the provided wallet to the specified address.
async fn send_reward(wallet: &kaspa_wallet_core::wallet::Wallet<InMemoryStorage>, addr: Address, amount: u64) -> Result<String> {
    let mut tx = wallet.create_transaction(&addr, amount).await?;
    let rpc = kaspa_rpc_core::client::RpcClient::new("http://127.0.0.1:16110")?;
    Ok(rpc.submit_transaction(tx.into()).await?)
}
