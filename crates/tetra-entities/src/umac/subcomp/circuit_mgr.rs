use std::collections::VecDeque;

use tetra_core::Direction;
use tetra_saps::control::call_control::Circuit;

const MAX_REALTIME_TX_BLOCKS_PER_SLOT: usize = 6;

pub struct CircuitMgr {
    pub dl: [Option<Circuit>; 4],
    pub ul: [Option<Circuit>; 4],

    /// Data blocks queued to be transmitted, per timeslot
    pub tx_data: [VecDeque<Vec<u8>>; 4],
    tx_data_dropped_oldest: [u64; 4],
}

impl CircuitMgr {
    pub fn new() -> Self {
        Self {
            dl: [None, None, None, None],
            ul: [None, None, None, None],
            tx_data: [VecDeque::new(), VecDeque::new(), VecDeque::new(), VecDeque::new()],
            tx_data_dropped_oldest: [0; 4],
        }
    }

    fn slot_index(ts: u8, context: &str) -> Option<usize> {
        if (1..=4).contains(&ts) {
            Some(ts as usize - 1)
        } else {
            tracing::warn!("UMAC CircuitMgr: {} called with invalid timeslot {}", context, ts);
            None
        }
    }

    fn traffic_slot_index(ts: u8, context: &str) -> Option<usize> {
        if (2..=4).contains(&ts) {
            Some(ts as usize - 1)
        } else {
            tracing::warn!("UMAC CircuitMgr: {} refused traffic circuit on timeslot {}", context, ts);
            None
        }
    }

    pub fn is_active(&self, dir: Direction, ts: u8) -> bool {
        let Some(slot) = Self::slot_index(ts, "is_active") else {
            return false;
        };
        match dir {
            Direction::Dl => self.dl[slot].is_some(),
            Direction::Ul => self.ul[slot].is_some(),
            _ => {
                tracing::error!("UMAC CircuitMgr: called with non-specific direction {:?}", dir);
                return Default::default();
            }
        }
    }

    pub fn get_usage(&self, dir: Direction, ts: u8) -> Option<u8> {
        let slot = Self::slot_index(ts, "get_usage")?;
        match dir {
            Direction::Dl => {
                if let Some(circuit) = &self.dl[slot] {
                    Some(circuit.usage)
                } else {
                    None
                }
            }
            Direction::Ul => {
                if let Some(circuit) = &self.ul[slot] {
                    Some(circuit.usage)
                } else {
                    None
                }
            }
            _ => {
                tracing::error!("UMAC CircuitMgr: called with non-specific direction {:?}", dir);
                return Default::default();
            }
        }
    }

    /// Closes an active circuit, and return the Circuit to the caller
    pub fn close_circuit(&mut self, dir: Direction, ts: u8) -> Option<Circuit> {
        let slot = Self::slot_index(ts, "close_circuit")?;
        match dir {
            Direction::Dl => {
                self.tx_data[slot].clear();
                self.dl[slot].take()
            }
            Direction::Ul => self.ul[slot].take(),
            _ => {
                tracing::error!("UMAC CircuitMgr: called with non-specific direction {:?}", dir);
                return Default::default();
            }
        }
    }

    /// Creates a new circuit on the given direction and timeslot
    /// This channel should be free, if not, warnings will be issued and the existing circuit will be closed first
    pub fn create_circuit(&mut self, dir: Direction, circuit: Circuit) {
        let ts = circuit.ts;
        let Some(slot) = Self::traffic_slot_index(ts, "create_circuit") else {
            return;
        };

        // Sanity check
        if self.is_active(dir, ts) {
            tracing::warn!("CircuitMgr::create had still active circuit on {:?} {}", dir, ts);
            self.close_circuit(dir, ts);
        }

        match dir {
            Direction::Dl => {
                if !self.tx_data[slot].is_empty() {
                    tracing::warn!("CircuitMgr::create had pending tx_data on Dl {}", ts);
                    self.tx_data[slot].clear();
                }
                self.dl[slot] = Some(circuit);
            }
            Direction::Ul => self.ul[slot] = Some(circuit),
            _ => {
                tracing::error!("UMAC CircuitMgr: called with non-specific direction {:?}", dir);
                return Default::default();
            }
        }
    }

    /// Put a block in the queue for transmission on an associated channel
    pub fn put_block(&mut self, ts: u8, block: Vec<u8>) {
        let Some(slot) = Self::slot_index(ts, "put_block") else {
            return;
        };
        if !self.is_active(Direction::Dl, ts) {
            tracing::warn!("CircuitMgr::put_block on inactive circuit {:?} {}", Direction::Dl, ts);
            return;
        }
        while self.tx_data[slot].len() >= MAX_REALTIME_TX_BLOCKS_PER_SLOT {
            self.tx_data[slot].pop_front();
            self.tx_data_dropped_oldest[slot] = self.tx_data_dropped_oldest[slot].saturating_add(1);
            if self.tx_data_dropped_oldest[slot] == 1 || self.tx_data_dropped_oldest[slot].is_power_of_two() {
                tracing::warn!(
                    ts,
                    dropped_oldest = self.tx_data_dropped_oldest[slot],
                    depth = self.tx_data[slot].len(),
                    max_depth = MAX_REALTIME_TX_BLOCKS_PER_SLOT,
                    "CircuitMgr::put_block dropped stale realtime TCH block"
                );
            }
        }
        self.tx_data[slot].push_back(block);
    }

    /// Take a to-be-transmitted block from the queue
    pub fn take_block(&mut self, ts: u8) -> Option<Vec<u8>> {
        let slot = Self::slot_index(ts, "take_block")?;
        if !self.is_active(Direction::Dl, ts) {
            tracing::warn!("CircuitMgr::take_block on inactive circuit {:?} {}", Direction::Dl, ts);
            return None;
        }
        self.tx_data[slot].pop_front()
    }
}
