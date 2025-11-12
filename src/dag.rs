use petgraph::{Graph, Directed, graph::NodeIndex};
use kaspa_consensus_core::block::Block;
use std::collections::{HashMap, VecDeque};
use anyhow::Result;

pub type Dag = Graph<BlockInfo, (), Directed>;

/// Contains essential information about a block in the DAG.
#[derive(Clone, Debug)]
pub struct BlockInfo {
    pub hash: String,
    pub blue_score: u64,
    pub parents: Vec<String>,
    pub timestamp: u64,
}

/// A rolling DAG with fixed capacity. When at capacity, the oldest blocks are evicted.
pub struct RollingDag {
    graph: Dag,
    idx: HashMap<String, NodeIndex>,
    order: VecDeque<String>,
    capacity: usize,
}

impl RollingDag {
    /// Creates a new RollingDag with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            graph: Graph::new(),
            idx: HashMap::new(),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Adds a block to the DAG.
    /// If the DAG is at capacity, the oldest block and its node index are evicted.
    /// Returns true if the block was added; false if it was already present.
    pub fn add_block(&mut self, block: Block) -> bool {
        let hash = block.hash().to_string();
        if self.idx.contains_key(&hash) {
            return false;
        }

        let info = BlockInfo {
            hash: hash.clone(),
            blue_score: block.header.blue_score,
            parents: block.header.direct_parents.iter().map(|h| h.to_string()).collect(),
            timestamp: block.header.timestamp,
        };

        // Evict oldest
        if self.order.len() >= self.capacity {
            if let Some(old_hash) = self.order.pop_front() {
                if let Some(&node) = self.idx.get(&old_hash) {
                    self.graph.remove_node(node);
                }
                self.idx.remove(&old_hash);
            }
        }

        let node = self.graph.add_node(info);

        self.idx.insert(hash.clone(), node);
        self.order.push_back(hash);

        for parent in &self.graph[node].parents {
            if let Some(&p_node) = self.idx.get(parent) {
                self.graph.add_edge(p_node, node, ());
            }
        }
        true
    }

    /// Finds a "fracture" point in the DAG where
    /// a node has at least two children whose blue score delta is >= min_delta,
    /// and prioritizes by betweenness centrality.
    pub fn find_fracture(&self, min_delta: u64) -> Option<(NodeIndex, Vec<NodeIndex>)> {
        use petgraph::algo::betweenness_centrality;
        let betweenness = betweenness_centrality(&self.graph);
        let mut candidates = vec![];

        for node in self.graph.node_indices() {
            let children: Vec<_> = self.graph.neighbors_directed(node, petgraph::Direction::Outgoing).collect();
            if children.len() < 2 { continue; }

            let info = &self.graph[node];
            let mut delta = u64::MAX;
            for &child in &children {
                let child_score = self.graph[child].blue_score;
                delta = delta.min(info.blue_score.abs_diff(child_score));
            }
            if delta < min_delta { continue; }

            candidates.push((node, betweenness[node.index()], delta));
        }

        // Sort by betweenness descending (higher is better), then by delta ascending (lower is better)
        candidates.sort_by(|a, b| {
            // Sort: betweenness descending, then delta ascending
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                .then(a.2.cmp(&b.2))
        });
        let best = candidates.first()?;
        let tips: Vec<_> = self.graph.neighbors_directed(best.0, petgraph::Direction::Outgoing).collect();
        Some((best.0, tips))
    }
}
