use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tetra_config::bluestation::SharedConfig;
use tetra_core::{TdmaTime, tetra_entities::TetraEntity};
use tetra_saps::SapMsg;

use crate::TetraEntityTrait;

const MAX_IMMEDIATE_MESSAGES_PER_DRAIN: usize = 4096;
const MAX_NORMAL_MESSAGES_PER_DRAIN: usize = 64;
const NORMAL_DELIVERY_BUDGET: Duration = Duration::from_millis(2);

#[derive(Default)]
pub enum MessagePrio {
    /// Intra-slot feedback that must preempt already queued immediate messages.
    Critical,
    Immediate,
    #[default]
    Normal,
}

pub struct MessageQueue {
    immediate_messages: VecDeque<SapMsg>,
    messages: VecDeque<SapMsg>,
}

impl MessageQueue {
    pub fn new() -> Self {
        Self {
            immediate_messages: VecDeque::new(),
            messages: VecDeque::new(),
        }
    }

    pub fn push_back(&mut self, message: SapMsg) {
        self.messages.push_back(message);
    }

    pub fn push_prio(&mut self, message: SapMsg, prio: MessagePrio) {
        match prio {
            MessagePrio::Critical => {
                self.immediate_messages.push_front(message);
            }
            MessagePrio::Immediate => {
                self.immediate_messages.push_back(message);
            }
            MessagePrio::Normal => {
                // Insert at the back for normal processing
                self.messages.push_back(message);
            }
        }
    }

    pub fn pop_front(&mut self) -> Option<SapMsg> {
        self.immediate_messages.pop_front().or_else(|| self.messages.pop_front())
    }

    fn pop_immediate(&mut self) -> Option<SapMsg> {
        self.immediate_messages.pop_front()
    }

    fn pop_normal(&mut self) -> Option<SapMsg> {
        self.messages.pop_front()
    }

    pub fn len(&self) -> usize {
        self.immediate_messages.len() + self.messages.len()
    }

    fn has_immediate(&self) -> bool {
        !self.immediate_messages.is_empty()
    }

    fn has_normal(&self) -> bool {
        !self.messages.is_empty()
    }
}

pub struct MessageRouter {
    /// While currently unused by the MessageRouter, this may change in the future
    /// As such, we provide the MessageRouter with a copy of the SharedConfig
    _config: SharedConfig,
    entities: HashMap<TetraEntity, Box<dyn TetraEntityTrait>>,
    msg_queue: MessageQueue,

    /// The current TDMA time, if applicable.
    /// For Bs mode, this is always available
    /// For Ms/Mon mode, it is recovered from a received SYNC frame and communicated in a different way
    ts: TdmaTime,
}

impl MessageRouter {
    pub fn new(config: SharedConfig) -> Self {
        Self {
            entities: HashMap::new(),
            msg_queue: MessageQueue::new(),
            _config: config,
            ts: TdmaTime::default(),
        }
    }

    /// For BS mode, sets global TDMA time
    /// Incremented each tick and passed to entities in tick() function
    pub fn set_dl_time(&mut self, ts: TdmaTime) {
        self.ts = ts;
    }

    pub fn register_entity(&mut self, entity: Box<dyn TetraEntityTrait>) {
        let comp_type = entity.entity();
        tracing::debug!("register_entity {:?}", comp_type);
        self.entities.insert(comp_type, entity);
    }

    /// Returns a mut ref to a component of the requested type
    pub fn get_entity(&mut self, comp: TetraEntity) -> Option<&mut dyn TetraEntityTrait> {
        self.entities.get_mut(&comp).map(|entity| entity.as_mut())
    }

    pub fn submit_message(&mut self, message: SapMsg) {
        tracing::debug!(
            "submit_message {:?}: {:?} -> {:?}",
            message.get_sap(),
            message.get_source(),
            message.get_dest()
        );
        self.msg_queue.push_back(message);
    }

    pub fn deliver_message(&mut self) {
        let message = self.msg_queue.pop_front();
        if let Some(message) = message {
            self.deliver_popped_message(message);
        }
    }

    fn deliver_popped_message(&mut self, message: SapMsg) {
        tracing::debug!(
            "deliver_message: got {:?}: {:?} -> {:?}",
            message.get_sap(),
            message.get_source(),
            message.get_dest()
        );

        // Determine the destination entity
        let dest = message.get_dest();

        // Check if the destination entity registered and deliver if found
        if let Some(entity) = self.entities.get_mut(dest) {
            entity.rx_prim(&mut self.msg_queue, message);
        } else {
            tracing::warn!(
                "deliver_message: entity {:?} not found for {:?}: {:?} -> {:?}",
                dest,
                message.get_sap(),
                message.get_source(),
                message.get_dest()
            );
        }
    }

    pub fn deliver_all_messages(&mut self) {
        let mut delivered = 0usize;
        while self.msg_queue.len() > 0 && delivered < MAX_IMMEDIATE_MESSAGES_PER_DRAIN {
            self.deliver_message();
            delivered += 1;
        }
        if self.msg_queue.len() > 0 {
            tracing::warn!(
                delivered,
                remaining = self.msg_queue.len(),
                max = MAX_IMMEDIATE_MESSAGES_PER_DRAIN,
                "MessageRouter: delivery budget exhausted; deferring messages to preserve TDMA timing"
            );
        }
    }

    pub fn deliver_immediate_messages(&mut self) {
        let mut delivered = 0usize;
        while delivered < MAX_IMMEDIATE_MESSAGES_PER_DRAIN {
            let Some(message) = self.msg_queue.pop_immediate() else {
                break;
            };
            self.deliver_popped_message(message);
            delivered += 1;
        }
        if self.msg_queue.has_immediate() {
            tracing::error!(
                delivered,
                remaining_immediate = self.msg_queue.immediate_messages.len(),
                max = MAX_IMMEDIATE_MESSAGES_PER_DRAIN,
                "MessageRouter: immediate delivery budget exhausted; TDMA timing may be affected"
            );
        }
    }

    pub fn deliver_normal_messages_for_tick(&mut self) {
        let started = Instant::now();
        let mut delivered = 0usize;
        while delivered < MAX_NORMAL_MESSAGES_PER_DRAIN && started.elapsed() < NORMAL_DELIVERY_BUDGET {
            if self.msg_queue.has_immediate() {
                self.deliver_immediate_messages();
                continue;
            }
            let Some(message) = self.msg_queue.pop_normal() else {
                break;
            };
            self.deliver_popped_message(message);
            delivered += 1;
        }
        if self.msg_queue.has_normal() {
            tracing::warn!(
                delivered,
                remaining_normal = self.msg_queue.messages.len(),
                max = MAX_NORMAL_MESSAGES_PER_DRAIN,
                budget_us = NORMAL_DELIVERY_BUDGET.as_micros(),
                "MessageRouter: normal delivery budget exhausted; deferring non-critical messages"
            );
        }
    }

    pub fn get_msgqueue_len(&self) -> usize {
        self.msg_queue.len()
    }

    pub fn tick_start(&mut self) {
        // tracing::info!("--- tick dl {} ul {} txdl {} ----------------------------",
        //     self.ts, self.ts.add_timeslots(-2), self.ts.add_timeslots(MACSCHED_TX_AHEAD as i32));
        tracing::info!("--- tick dl {} ----------------------------", self.ts);

        // RF-critical tick_start path first. UMAC finalizes the future DL timeslot,
        // LMAC encodes it, and PHY performs the timed TX/RX. Immediate delivery
        // keeps this lane ahead of Brew/dashboard/telemetry backlog.
        for target in [TetraEntity::Phy, TetraEntity::Lmac, TetraEntity::Umac] {
            if let Some(entity) = self.entities.get_mut(&target) {
                entity.tick_start(&mut self.msg_queue, self.ts);
            }
            self.deliver_immediate_messages();
        }

        // Call tick on all remaining entities
        let remaining: Vec<TetraEntity> = self
            .entities
            .keys()
            .copied()
            .filter(|entity_id| !matches!(entity_id, TetraEntity::Phy | TetraEntity::Lmac | TetraEntity::Umac))
            .collect();
        for entity_id in remaining {
            if let Some(entity) = self.entities.get_mut(&entity_id) {
                entity.tick_start(&mut self.msg_queue, self.ts);
            }
            self.deliver_immediate_messages();
        }
    }

    /// Executes all end-of-tick functions:
    /// - LLC sends down all outstanding BL-ACKs
    /// - UMAC finalizes any resources for ts and sends down to LMAC
    ///
    pub fn tick_end(&mut self) {
        tracing::debug!("############################ end-of-tick ############################");

        // Llc should send down outstanding BL-ACKs
        let target = TetraEntity::Llc;
        if let Some(entity) = self.entities.get_mut(&target) {
            tracing::trace!("tick_end for entity {:?}", target);
            entity.tick_end(&mut self.msg_queue, self.ts);
        }
        self.deliver_immediate_messages();

        // Umac should finalize any resources and send down to Lmac
        let target = TetraEntity::Umac;
        if let Some(entity) = self.entities.get_mut(&target) {
            tracing::trace!("tick_end for entity {:?}", target);
            entity.tick_end(&mut self.msg_queue, self.ts);
        }
        self.deliver_immediate_messages();

        // Then call tick_end on all other entities
        for entity in self.entities.values_mut() {
            let entity_id = entity.entity();
            if entity_id == TetraEntity::Llc || entity_id == TetraEntity::Umac {
                continue;
            }
            entity.tick_end(&mut self.msg_queue, self.ts);
        }
        self.deliver_immediate_messages();

        // Increment the TDMA time if set
        self.ts = self.ts.add_timeslots(1);
    }

    /// Runs the full stack either forever or for a specified number of ticks.
    /// If `running` is provided, the loop will exit when the flag is set to false
    /// (e.g. by a Ctrl+C signal handler), allowing entities to be dropped cleanly.
    pub fn run_stack(&mut self, num_ticks: Option<usize>, running: Option<Arc<AtomicBool>>) {
        let mut ticks: usize = 0;

        loop {
            // Check if we've been asked to stop (e.g. Ctrl+C)
            if let Some(ref flag) = running {
                if !flag.load(Ordering::Relaxed) {
                    eprintln!("\n[INFO] Shutting down gracefully...");
                    break;
                }
            }

            // Send tick_start event
            self.tick_start();

            if num_ticks.is_some() {
                // Preserve deterministic test/simulation semantics: inputs submitted
                // before a finite tick are delivered before entity tick_end hooks run.
                self.deliver_all_messages();
            }

            // Send tick_end event and process final messages
            self.tick_end();

            if num_ticks.is_some() {
                // Finite stack runs are used by deterministic component tests and offline
                // simulations; drain fully so assertions observe all generated SAPs.
                self.deliver_all_messages();
            } else {
                self.deliver_normal_messages_for_tick();
            }

            // Check if we should stop
            ticks += 1;
            if let Some(num_ticks) = num_ticks {
                if ticks >= num_ticks {
                    break;
                }
            }
        }
    }
}
