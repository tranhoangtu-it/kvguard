//! KV slot routing for llama.cpp `llama-server`.
//!
//! The main conversation is pinned to slot 0; sub-agent branches share the remaining
//! slots with LRU eviction, so a short sub-agent prompt never overwrites the main
//! agent's long KV cache.

use serde_json::Value;

/// Tracks which conversation branch owns each llama-server slot.
#[derive(Debug, Clone)]
pub struct SlotPool {
    owners: Vec<Option<u64>>,
    last_used: Vec<u64>,
}

impl SlotPool {
    /// Returns `None` when there is nothing to route (fewer than two slots).
    pub fn new(slots: u32) -> Option<Self> {
        (slots >= 2).then(|| Self {
            owners: vec![None; slots as usize],
            last_used: vec![0; slots as usize],
        })
    }

    pub fn slot_count(&self) -> usize {
        self.owners.len()
    }

    /// Assign a slot to `branch_id`. `clock` is a monotonically increasing request counter.
    pub fn assign(&mut self, branch_id: u64, is_main: bool, clock: u64) -> u32 {
        let secondary = 1..self.owners.len();
        let slot = if is_main {
            // A branch promoted to main gives up any secondary slot it held.
            for owner in self.owners.iter_mut().skip(1) {
                if *owner == Some(branch_id) {
                    *owner = None;
                }
            }
            0
        } else if let Some(owned) = secondary
            .clone()
            .find(|&i| self.owners[i] == Some(branch_id))
        {
            owned
        } else {
            secondary
                .min_by_key(|&i| (self.owners[i].is_some(), self.last_used[i]))
                .unwrap_or(1)
        };
        self.owners[slot] = Some(branch_id);
        self.last_used[slot] = clock;
        slot as u32
    }
}

/// Insert llama-server routing fields into an OpenAI-compatible request body.
pub fn inject_slot(body: &mut Value, slot: u32) -> bool {
    let Some(map) = body.as_object_mut() else {
        return false;
    };
    map.insert("id_slot".into(), Value::from(slot));
    map.insert("cache_prompt".into(), Value::Bool(true));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_slot_server_is_not_routed() {
        assert!(SlotPool::new(1).is_none());
    }

    #[test]
    fn main_gets_slot_zero_and_subagents_rotate_lru() {
        let mut pool = SlotPool::new(3).expect("pool");
        assert_eq!(pool.assign(1, true, 1), 0);
        assert_eq!(pool.assign(2, false, 2), 1);
        assert_eq!(pool.assign(3, false, 3), 2);
        assert_eq!(pool.assign(2, false, 4), 1, "existing owner keeps its slot");
        assert_eq!(pool.assign(4, false, 5), 2, "LRU slot is evicted");
        assert_eq!(pool.assign(1, true, 6), 0);
    }
}
