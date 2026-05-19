use tetra_pdus::cmce::fields::ss_tpi::SsTpiInform;

use super::*;
use crate::net_identity::{IdentityRecord, IdentityResolver, normalize_mnemonic};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MotorolaTpiTestMode {
    Off,
    TxGranted,
    SetupMnemonicOnly,
    SetupPulse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TpiCallType {
    Group,
    IndividualHalfDuplex,
    IndividualFullDuplex,
}

#[derive(Debug, Clone)]
pub(super) struct TpiCallContext {
    pub(super) call_type: TpiCallType,
    pub(super) origin_issi: u32,
    pub(super) current_talker_issi: u32,
    pub(super) talker_mnemonic: Option<String>,
    pub(super) clir_invoked: bool,
}

impl CcBsSubentity {
    pub(super) fn tpi_resolver_for_config(config: &SharedConfig) -> IdentityResolver {
        IdentityResolver::new(&config.config().identity)
    }

    pub(super) fn tpi_start_context(&mut self, call_id: u16, call_type: TpiCallType, origin_issi: u32, clir_invoked: bool) {
        if !self.config.config().identity.enabled {
            return;
        }

        let talker_mnemonic = if clir_invoked {
            None
        } else {
            self.tpi_resolve_mnemonic(origin_issi)
        };

        self.tpi_contexts.insert(
            call_id,
            TpiCallContext {
                call_type,
                origin_issi,
                current_talker_issi: origin_issi,
                talker_mnemonic,
                clir_invoked,
            },
        );
    }

    pub(super) fn tpi_update_talker(&mut self, call_id: u16, talker_issi: u32) {
        if !self.config.config().identity.enabled {
            return;
        }

        let Some(ctx) = self.tpi_contexts.get(&call_id) else {
            return;
        };
        if ctx.current_talker_issi == talker_issi {
            return;
        }

        let talker_mnemonic = if ctx.clir_invoked {
            None
        } else {
            self.tpi_resolve_mnemonic(talker_issi)
        };

        if let Some(ctx) = self.tpi_contexts.get_mut(&call_id) {
            ctx.current_talker_issi = talker_issi;
            ctx.talker_mnemonic = talker_mnemonic;
        }
    }

    pub(super) fn tpi_inform_for_call(&self, call_id: u16) -> Option<SsTpiInform> {
        let cfg = self.config.config();
        if !cfg.identity.enabled {
            return None;
        }

        let ctx = self.tpi_contexts.get(&call_id)?;
        if ctx.clir_invoked {
            return Some(SsTpiInform::clir());
        }

        let mnemonic = if cfg.identity.emit_mnemonic_name && cfg.identity.subscription_allows_mnemonic {
            ctx.talker_mnemonic.clone()
        } else {
            None
        };

        match (ctx.call_type, mnemonic) {
            // For group call D-SETUP, the talking/sending party identity is
            // already encoded as calling_party_address_ssi. SS-TPI only needs
            // to add the mnemonic name for RX terminal display.
            (TpiCallType::Group, Some(mnemonic)) => Some(SsTpiInform::mnemonic_only(mnemonic)),
            (TpiCallType::Group, None) => None,
            (_, mnemonic) => Some(SsTpiInform::for_ssi(ctx.current_talker_issi, mnemonic)),
        }
    }

    pub(super) fn tpi_for_speaker(&mut self, call_id: u16, talker_issi: u32) -> Option<SsTpiInform> {
        self.tpi_update_talker(call_id, talker_issi);
        self.tpi_inform_for_call(call_id)
    }

    pub(super) fn tpi_end_context(&mut self, call_id: u16) {
        self.tpi_contexts.remove(&call_id);
    }

    pub(super) fn motorola_tpi_test_mode(&self, dest_gssi: u32) -> MotorolaTpiTestMode {
        let target_gssi = std::env::var("FLOWSTATION_MOTOROLA_TPI_GSSI")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok());
        if target_gssi != Some(dest_gssi) {
            return MotorolaTpiTestMode::Off;
        }

        match std::env::var("FLOWSTATION_MOTOROLA_TPI_TEST")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "a" | "tx-granted" | "d-tx-granted" => MotorolaTpiTestMode::TxGranted,
            "b" | "setup-mnemonic-only" | "mnemonic-only" => MotorolaTpiTestMode::SetupMnemonicOnly,
            "c" | "setup-pulse" | "pulse" => MotorolaTpiTestMode::SetupPulse,
            _ => MotorolaTpiTestMode::Off,
        }
    }

    pub(super) fn motorola_tpi_setup_suppresses_caller(&self, dest_gssi: u32) -> bool {
        matches!(
            self.motorola_tpi_test_mode(dest_gssi),
            MotorolaTpiTestMode::SetupMnemonicOnly | MotorolaTpiTestMode::SetupPulse
        )
    }

    pub(super) fn motorola_tpi_tx_granted_facility(&self, call_id: u16, dest_gssi: u32) -> Option<SsTpiInform> {
        match self.motorola_tpi_test_mode(dest_gssi) {
            MotorolaTpiTestMode::TxGranted | MotorolaTpiTestMode::SetupPulse => self.tpi_inform_for_call(call_id),
            MotorolaTpiTestMode::Off | MotorolaTpiTestMode::SetupMnemonicOnly => None,
        }
    }

    pub(super) fn motorola_tpi_emits_initial_tx_granted(&self, dest_gssi: u32) -> bool {
        matches!(
            self.motorola_tpi_test_mode(dest_gssi),
            MotorolaTpiTestMode::TxGranted | MotorolaTpiTestMode::SetupPulse
        )
    }

    fn tpi_resolve_mnemonic(&self, issi: u32) -> Option<String> {
        self.identity_resolver.lookup(issi).and_then(tpi_mnemonic_from_record)
    }
}

fn tpi_mnemonic_from_record(record: IdentityRecord) -> Option<String> {
    let IdentityRecord { mnemonic, label, .. } = record;
    mnemonic.or_else(|| label.as_deref().and_then(normalize_mnemonic))
}
