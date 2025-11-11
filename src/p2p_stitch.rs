use kaspa_p2p_lib::{Adaptor, Hub, ConnectionInitializer, Peer, Router, common::ProtocolError, Flow, FlowContext};
use secp256k1::{SecretKey, PublicKey, Message, ecdsa::Signature};
use bincode::{serialize, deserialize};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use chrono::Utc;

const STITCH_MSG_ID: u8 = 0xF0;

#[derive(Clone, Serialize, Deserialize)]
pub struct StitchRequest {
    pub weak_block: String,
    pub tip_hashes: Vec<String>,
    pub reward_sompi: u64,
    pub expiry: u64,
    pub signature: Vec<u8>,
    pub pubkey: Vec<u8>,
}

impl StitchRequest {
    pub fn new(weak: &str, tips: &[String], reward: u64, sk: &SecretKey) -> Self {
        let pubkey = PublicKey::from_secret_key(sk);
        let mut req = Self {
            weak_block: weak.to_string(),
            tip_hashes: tips.to_vec(),
            reward_sompi: reward,
            expiry: (Utc::now().timestamp() as u64) + 30,
            signature: vec![],
            pubkey: pubkey.serialize().to_vec(),
        };
        req.signature = req.sign(sk);
        req
    }

    fn hash(&self) -> Message {
        let data = serialize(&(&self.weak_block, &self.tip_hashes, self.reward_sompi, self.expiry)).unwrap();
        Message::from_slice(&blake2s_simd::hash(&data).as_bytes()).unwrap()
    }

    fn sign(&self, sk: &SecretKey) -> Vec<u8> {
        let msg = self.hash();
        sk.sign_ecdsa(&msg).serialize_compact().to_vec()
    }

    pub fn verify(&self) -> bool {
        let pubkey = PublicKey::from_slice(&self.pubkey).ok()?;
        let sig = Signature::from_compact(&self.signature).ok()?;
        let msg = self.hash();
        pubkey.verify(&msg, &sig).is_ok()
    }
}

pub async fn broadcast_stitch(
    adaptor: &Adaptor,
    weak: &str,
    tips: &[String],
    reward: u64,
    sk: &SecretKey,
) -> Result<()> {
    let req = StitchRequest::new(weak, tips, reward, sk);
    let payload = serialize(&req)?;
    let msg = kaspa_p2p_lib::common::Message::new(STITCH_MSG_ID, payload);
    adaptor.hub().broadcast(msg).await?;
    Ok(())
}

pub async fn setup_p2p(cfg: &super::config::Config) -> Result<Adaptor> {
    let initializer = Arc::new(StitchInitializer::new());
    let mut adaptor = Adaptor::new(initializer);
    for peer in &cfg.p2p_bootstrap_peers {
        adaptor.connect(peer).await?;
    }
    Ok(adaptor)
}

struct StitchInitializer;

impl StitchInitializer {
    fn new() -> Self { Self }
}

impl ConnectionInitializer for StitchInitializer {
    fn initialize(&self, peer: Arc<Peer>) -> Result<Router, ProtocolError> {
        let mut router = Router::new(peer);
        router.register_flow(STITCH_MSG_ID, Arc::new(StitchFlow));
        Ok(router)
    }
}

struct StitchFlow;

impl Flow for StitchFlow {
    fn router(&self) -> Option<Arc<Router>> { None }
    fn start(&mut self, mut ctx: FlowContext) {
        tokio::spawn(async move {
            while let Some(msg) = ctx.recv().await {
                if msg.id == STITCH_MSG_ID {
                    if let Ok(req) = deserialize::<StitchRequest>(&msg.payload) {
                        if req.verify() && req.expiry > Utc::now().timestamp() as u64 {
                            log::info!("Valid stitch request: {}", req.weak_block);
                            // Miner: add parents to block template
                        }
                    }
                }
            }
        });
    }
}
