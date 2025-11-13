use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub rpc_url: String,
    pub p2p_port: u16,
    pub p2p_bootstrap_peers: Vec<String>,
    pub adaptive: bool,
    pub base_min_delta: u64,
    pub base_rate_limit: u64,
    pub base_reward_sompi: u64,
    pub max_reward_sompi: u64,
    pub min_rate_limit: u64,
    pub dag_window: usize,
}

impl Config {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}
