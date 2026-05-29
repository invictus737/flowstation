mod common;

use std::time::Duration;

use tetra_config::bluestation::{CfgBrew, CfgIdentity, CfgManualIdentity, StackMode};
use tetra_core::tetra_entities::TetraEntity;
use tetra_core::{BitBuffer, Sap, SsiType, TdmaTime, TetraAddress, TxState, debug};
use tetra_pdus::cmce::enums::disconnect_cause::DisconnectCause;
use tetra_pdus::cmce::enums::party_type_identifier::PartyTypeIdentifier;
use tetra_pdus::cmce::enums::transmission_grant::TransmissionGrant;
use tetra_pdus::cmce::fields::basic_service_information::BasicServiceInformation;
use tetra_pdus::cmce::pdus::d_release::DRelease;
use tetra_pdus::cmce::pdus::d_setup::DSetup;
use tetra_pdus::cmce::pdus::d_tx_ceased::DTxCeased;
use tetra_pdus::cmce::pdus::d_tx_granted::DTxGranted;
use tetra_pdus::cmce::pdus::u_connect::UConnect;
use tetra_pdus::cmce::pdus::u_facility::UFacility;
use tetra_pdus::cmce::pdus::u_release::URelease;
use tetra_pdus::cmce::pdus::u_setup::USetup;
use tetra_pdus::cmce::pdus::u_tx_ceased::UTxCeased;
use tetra_pdus::cmce::pdus::u_tx_demand::UTxDemand;
use tetra_saps::control::brew::{BrewSubscriberAction, MmSubscriberUpdate};
use tetra_saps::control::call_control::CallControl;
use tetra_saps::control::enums::circuit_mode_type::CircuitModeType;
use tetra_saps::control::enums::communication_type::CommunicationType;
use tetra_saps::lcmc::LcmcMleUnitdataInd;
use tetra_saps::sapmsg::{SapMsg, SapMsgInner};

use crate::common::ComponentTest;

const TEST_GSSI: u32 = 91;
const TEST_ISSI: u32 = 1000001;
const TEST_ISSI_2: u32 = 1000002;
const TEST_ISSI_3: u32 = 1000003;

/// Helper: register a subscriber on a GSSI so CMCE accepts calls for that group.
fn register_subscriber(test: &mut ComponentTest, issi: u32, gssi: u32) {
    let register = SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Mm,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::MmSubscriberUpdate(MmSubscriberUpdate {
            issi,
            groups: vec![],
            action: BrewSubscriberAction::Register,
        }),
    };
    test.submit_message(register);
    test.run_stack(Some(2));

    let affiliate = SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Mm,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::MmSubscriberUpdate(MmSubscriberUpdate {
            issi,
            groups: vec![gssi],
            action: BrewSubscriberAction::Affiliate,
        }),
    };
    test.submit_message(affiliate);
    test.run_stack(Some(1));
    test.dump_sinks();
}

/// Helper: build a U-SETUP SAP message for a group call.
fn build_u_setup_msg(calling_issi: u32, dest_gssi: u32) -> SapMsg {
    let u_setup = USetup {
        area_selection: 0,
        hook_method_selection: false,
        simplex_duplex_selection: false,
        basic_service_information: BasicServiceInformation {
            circuit_mode_type: CircuitModeType::TchS,
            encryption_flag: false,
            communication_type: CommunicationType::P2Mp,
            slots_per_frame: None,
            speech_service: Some(0),
        },
        request_to_transmit_send_data: false,
        call_priority: 0,
        clir_control: 0,
        called_party_type_identifier: PartyTypeIdentifier::Ssi,
        called_party_ssi: Some(dest_gssi as u64),
        called_party_short_number_address: None,
        called_party_extension: None,
        external_subscriber_number: None,
        facility: None,
        dm_ms_address: None,
        proprietary: None,
    };

    let mut sdu = BitBuffer::new_autoexpand(80);
    u_setup.to_bitbuf(&mut sdu).expect("Failed to serialize USetup");
    sdu.seek(0);

    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(calling_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

fn build_u_setup_p2p_msg(calling_issi: u32, called_issi: u32, simplex_duplex_selection: bool, call_priority: u8) -> SapMsg {
    let u_setup = USetup {
        area_selection: 0,
        hook_method_selection: false,
        simplex_duplex_selection,
        basic_service_information: BasicServiceInformation {
            circuit_mode_type: CircuitModeType::TchS,
            encryption_flag: false,
            communication_type: CommunicationType::P2p,
            slots_per_frame: None,
            speech_service: Some(0),
        },
        request_to_transmit_send_data: false,
        call_priority,
        clir_control: 0,
        called_party_type_identifier: PartyTypeIdentifier::Ssi,
        called_party_ssi: Some(called_issi as u64),
        called_party_short_number_address: None,
        called_party_extension: None,
        external_subscriber_number: None,
        facility: None,
        dm_ms_address: None,
        proprietary: None,
    };

    let mut sdu = BitBuffer::new_autoexpand(80);
    u_setup.to_bitbuf(&mut sdu).expect("Failed to serialize USetup");
    sdu.seek(0);

    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(calling_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

/// Extract tx_reporters from D-SETUP messages in the sink output.
/// D-SETUPs are identified as LcmcMleUnitdataReq with a chan_alloc that has a usage field.
fn extract_d_setup_reporters(msgs: &mut Vec<SapMsg>) -> Vec<tetra_core::TxReporter> {
    let mut reporters = vec![];
    for msg in msgs.iter_mut() {
        if msg.dest == TetraEntity::Mle {
            if let SapMsgInner::LcmcMleUnitdataReq(ref mut prim) = msg.msg {
                if prim.chan_alloc.as_ref().is_some_and(|ca| ca.usage.is_some()) {
                    if let Some(reporter) = prim.tx_reporter.take() {
                        reporters.push(reporter);
                    }
                }
            }
        }
    }
    reporters
}

/// Count D-SETUP messages in sink output without taking reporters.
fn count_d_setups(msgs: &[SapMsg]) -> usize {
    parsed_d_setups(msgs).len()
}

fn parsed_d_setups(msgs: &[SapMsg]) -> Vec<DSetup> {
    msgs.iter()
        .filter_map(|msg| {
            if msg.dest != TetraEntity::Mle {
                return None;
            }
            let SapMsgInner::LcmcMleUnitdataReq(prim) = &msg.msg else {
                return None;
            };
            if !prim.chan_alloc.as_ref().is_some_and(|ca| ca.usage.is_some()) {
                return None;
            }
            let mut sdu = BitBuffer::from_bitstr(&prim.sdu.to_bitstr());
            DSetup::from_bitbuf(&mut sdu).ok()
        })
        .collect()
}

fn parsed_d_setups_any(msgs: &[SapMsg]) -> Vec<DSetup> {
    msgs.iter()
        .filter_map(|msg| {
            if msg.dest != TetraEntity::Mle {
                return None;
            }
            let SapMsgInner::LcmcMleUnitdataReq(prim) = &msg.msg else {
                return None;
            };
            let mut sdu = BitBuffer::from_bitstr(&prim.sdu.to_bitstr());
            DSetup::from_bitbuf(&mut sdu).ok()
        })
        .collect()
}

fn parsed_d_releases(msgs: &[SapMsg]) -> Vec<DRelease> {
    msgs.iter()
        .filter_map(|msg| {
            if msg.dest != TetraEntity::Mle {
                return None;
            }
            let SapMsgInner::LcmcMleUnitdataReq(prim) = &msg.msg else {
                return None;
            };
            let mut sdu = BitBuffer::from_bitstr(&prim.sdu.to_bitstr());
            DRelease::from_bitbuf(&mut sdu).ok()
        })
        .collect()
}

fn parsed_d_tx_granted_to(msgs: &[SapMsg], issi: u32) -> Vec<DTxGranted> {
    msgs.iter()
        .filter_map(|msg| {
            if msg.dest != TetraEntity::Mle {
                return None;
            }
            let SapMsgInner::LcmcMleUnitdataReq(prim) = &msg.msg else {
                return None;
            };
            if prim.main_address.ssi != issi {
                return None;
            }
            let mut sdu = BitBuffer::from_bitstr(&prim.sdu.to_bitstr());
            DTxGranted::from_bitbuf(&mut sdu).ok()
        })
        .collect()
}

fn parsed_d_tx_granted_for_address(msgs: &[SapMsg], ssi: u32) -> Vec<DTxGranted> {
    msgs.iter()
        .filter_map(|msg| {
            if msg.dest != TetraEntity::Mle {
                return None;
            }
            let SapMsgInner::LcmcMleUnitdataReq(prim) = &msg.msg else {
                return None;
            };
            if prim.main_address.ssi != ssi {
                return None;
            }
            let mut sdu = BitBuffer::from_bitstr(&prim.sdu.to_bitstr());
            DTxGranted::from_bitbuf(&mut sdu).ok()
        })
        .collect()
}

fn parsed_d_tx_ceased(msgs: &[SapMsg]) -> Vec<DTxCeased> {
    msgs.iter()
        .filter_map(|msg| {
            if msg.dest != TetraEntity::Mle {
                return None;
            }
            let SapMsgInner::LcmcMleUnitdataReq(prim) = &msg.msg else {
                return None;
            };
            let mut sdu = BitBuffer::from_bitstr(&prim.sdu.to_bitstr());
            DTxCeased::from_bitbuf(&mut sdu).ok()
        })
        .collect()
}

fn build_u_tx_ceased_msg(calling_issi: u32, call_id: u16) -> SapMsg {
    let mut sdu = BitBuffer::new_autoexpand(24);
    UTxCeased {
        call_identifier: call_id,
        facility: None,
        dm_ms_address: None,
        proprietary: None,
    }
    .to_bitbuf(&mut sdu)
    .expect("Failed to serialize UTxCeased");
    sdu.seek(0);

    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(calling_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

fn build_u_tx_demand_msg(calling_issi: u32, call_id: u16, priority: u8) -> SapMsg {
    let mut sdu = BitBuffer::new_autoexpand(24);
    UTxDemand {
        call_identifier: call_id,
        tx_demand_priority: priority,
        encryption_control: false,
        reserved: false,
        facility: None,
        dm_ms_address: None,
        proprietary: None,
    }
    .to_bitbuf(&mut sdu)
    .expect("Failed to serialize UTxDemand");
    sdu.seek(0);

    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(calling_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

fn build_u_connect_msg(called_issi: u32, call_id: u16, simplex_duplex_selection: bool) -> SapMsg {
    let mut sdu = BitBuffer::new_autoexpand(32);
    UConnect {
        call_identifier: call_id,
        hook_method_selection: false,
        simplex_duplex_selection,
        basic_service_information: None,
        facility: None,
        proprietary: None,
    }
    .to_bitbuf(&mut sdu)
    .expect("Failed to serialize UConnect");
    sdu.seek(0);

    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 2,
            endpoint_id: 2,
            link_id: 2,
            received_tetra_address: TetraAddress::new(called_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

fn build_u_release_msg(calling_issi: u32, call_id: u16) -> SapMsg {
    let mut sdu = BitBuffer::new_autoexpand(32);
    URelease {
        call_identifier: call_id,
        disconnect_cause: DisconnectCause::UserRequestedDisconnection,
        facility: None,
        proprietary: None,
    }
    .to_bitbuf(&mut sdu)
    .expect("Failed to serialize URelease");
    sdu.seek(0);

    SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(calling_issi, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    }
}

#[test]
fn test_u_facility_probe_has_no_error_response() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Cmce], vec![TetraEntity::Mle, TetraEntity::Mm]);

    let mut sdu = BitBuffer::new_autoexpand(16);
    UFacility {}.to_bitbuf(&mut sdu).expect("Failed to serialize UFacility");
    sdu.seek(0);

    test.submit_message(SapMsg {
        sap: Sap::LcmcSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::LcmcMleUnitdataInd(LcmcMleUnitdataInd {
            sdu,
            handle: 1,
            endpoint_id: 1,
            link_id: 1,
            received_tetra_address: TetraAddress::new(TEST_ISSI, SsiType::Issi),
            chan_change_resp_req: false,
            chan_change_handle: None,
        }),
    });
    test.run_stack(Some(1));

    let msgs = test.dump_sinks();
    assert!(
        !msgs
            .iter()
            .any(|msg| msg.dest == TetraEntity::Mle && matches!(msg.msg, SapMsgInner::LcmcMleUnitdataReq(_))),
        "U-FACILITY probes must not get D-CMCE-FUNCTION-NOT-SUPPORTED"
    );
    assert!(
        msgs.iter().any(|msg| {
            msg.dest == TetraEntity::Mm
                && matches!(
                    &msg.msg,
                    SapMsgInner::MmForceLocationUpdate { issi, .. } if *issi == TEST_ISSI
                )
        }),
        "Unknown U-FACILITY probes should force MM location update"
    );
}

/// Test that late-entry D-SETUP re-sends are throttled when the previous
/// D-SETUP's TxReceipt is still in Pending state (UMAC hasn't transmitted it yet),
/// and that they resume once the receipt reaches a final state.
#[test]
fn test_dsetup_late_entry_throttle() {
    debug::setup_logging_verbose();

    // Start at timeslot 1 so circuit creation aligns cleanly with tick_start checks
    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut cfg = ComponentTest::get_default_test_config(StackMode::Bs);
    cfg.brew = Some(CfgBrew {
        host: "test-brew.local".to_string(),
        port: 443,
        tls: false,
        username: None,
        password: None,
        reconnect_delay: Duration::from_secs(1),
        jitter_initial_latency_frames: 0,
        feature_sds_enabled: true,
        feature_rssi_export: false,
        whitelisted_ssis: None,
    });
    let mut test = ComponentTest::from_config(cfg, Some(dltime));

    let components = vec![TetraEntity::Cmce];
    let sinks = vec![TetraEntity::Mle, TetraEntity::Umac, TetraEntity::Brew];
    test.populate_entities(components, sinks);

    register_subscriber(&mut test, TEST_ISSI, TEST_GSSI);

    let brew_uuid = uuid::Uuid::from_u128(0x92);
    test.submit_message(SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Brew,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::CmceCallControl(CallControl::NetworkCallStart {
            brew_uuid,
            source_issi: TEST_ISSI_2,
            dest_gssi: TEST_GSSI,
            priority: 0,
        }),
    });
    test.run_stack(Some(2));

    // Collect initial output — should contain D-SETUP (initial send with no tracked receipt)
    let mut initial_msgs = test.dump_sinks();
    let initial_setups = count_d_setups(&initial_msgs);
    assert!(initial_setups > 0, "Expected initial D-SETUP after network call start");

    // Run until the first resend creates a tracked receipt.
    let mut backup_reporters = extract_d_setup_reporters(&mut initial_msgs);
    for _ in 0..25 {
        if !backup_reporters.is_empty() {
            break;
        }
        test.run_stack(Some(20));
        let mut backup_msgs = test.dump_sinks();
        backup_reporters = extract_d_setup_reporters(&mut backup_msgs);
    }

    // We should have at least one reporter from the backup send
    assert!(!backup_reporters.is_empty(), "Expected early late-entry D-SETUP with tx_reporter");
    let last_reporter = &backup_reporters[backup_reporters.len() - 1];
    assert_eq!(last_reporter.get_state(), TxState::Pending);

    // Run for 2 full late-entry intervals (720 ticks). With the receipt still Pending,
    // ALL late-entry D-SETUPs should be suppressed.
    test.run_stack(Some(720));
    let throttled_msgs = test.dump_sinks();
    let throttled_count = count_d_setups(&throttled_msgs);
    assert_eq!(
        throttled_count, 0,
        "Late-entry D-SETUPs should be suppressed while receipt is Pending"
    );

    // Now mark the previous D-SETUP as transmitted (simulating UMAC sending it over the air)
    last_reporter.mark_transmitted();

    // Run for 2 more late-entry intervals. Now D-SETUPs should go through.
    test.run_stack(Some(720));
    let mut unthrottled_msgs = test.dump_sinks();
    let unthrottled_count = count_d_setups(&unthrottled_msgs);
    assert!(
        unthrottled_count > 0,
        "Late-entry D-SETUPs should resume once receipt reaches final state"
    );

    // Each late-entry re-send is tracked so pending MCCH acquisition does not build
    // an unbounded queue.
    let new_reporters = extract_d_setup_reporters(&mut unthrottled_msgs);
    assert_eq!(
        new_reporters.len(),
        unthrottled_count,
        "Each re-sent D-SETUP should carry a fresh tx_reporter"
    );
}

#[test]
fn test_network_group_call_has_early_late_entry_dsetup_for_short_calls() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut cfg = ComponentTest::get_default_test_config(StackMode::Bs);
    cfg.brew = Some(CfgBrew {
        host: "test-brew.local".to_string(),
        port: 443,
        tls: false,
        username: None,
        password: None,
        reconnect_delay: Duration::from_secs(1),
        jitter_initial_latency_frames: 0,
        feature_sds_enabled: true,
        feature_rssi_export: false,
        whitelisted_ssis: None,
    });
    let mut test = ComponentTest::from_config(cfg, Some(dltime));
    test.populate_entities(
        vec![TetraEntity::Cmce],
        vec![TetraEntity::Mle, TetraEntity::Umac, TetraEntity::Brew],
    );

    register_subscriber(&mut test, TEST_ISSI, TEST_GSSI);

    let brew_uuid = uuid::Uuid::from_u128(0x91);
    test.submit_message(SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Brew,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::CmceCallControl(CallControl::NetworkCallStart {
            brew_uuid,
            source_issi: TEST_ISSI_2,
            dest_gssi: TEST_GSSI,
            priority: 0,
        }),
    });
    test.run_stack(Some(2));
    let mut initial_msgs = test.dump_sinks();
    let call_id = parsed_d_setups(&initial_msgs)
        .first()
        .map(|setup| setup.call_identifier)
        .expect("network group call must emit initial D-SETUP");
    for reporter in extract_d_setup_reporters(&mut initial_msgs) {
        reporter.mark_transmitted();
    }

    test.run_stack(Some(8));
    let mut backup_msgs = test.dump_sinks();
    for reporter in extract_d_setup_reporters(&mut backup_msgs) {
        reporter.mark_transmitted();
    }

    test.run_stack(Some(120));
    let early_setups: Vec<_> = parsed_d_setups(&test.dump_sinks())
        .into_iter()
        .filter(|setup| setup.call_identifier == call_id)
        .collect();

    assert!(
        !early_setups.is_empty(),
        "network group calls need an early late-entry D-SETUP before the 5-multiframe cadence"
    );
}

#[test]
fn test_network_group_speaker_change_refreshes_dsetup_for_scan_late_entry() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut cfg = ComponentTest::get_default_test_config(StackMode::Bs);
    cfg.identity = CfgIdentity {
        enabled: true,
        manual: vec![
            CfgManualIdentity {
                ssi: TEST_ISSI_2,
                mnemonic: Some("ISSI2".to_string()),
                label: None,
            },
            CfgManualIdentity {
                ssi: TEST_ISSI_3,
                mnemonic: Some("ISSI3".to_string()),
                label: None,
            },
        ],
        ..CfgIdentity::default()
    };
    cfg.brew = Some(CfgBrew {
        host: "test-brew.local".to_string(),
        port: 443,
        tls: false,
        username: None,
        password: None,
        reconnect_delay: Duration::from_secs(1),
        jitter_initial_latency_frames: 0,
        feature_sds_enabled: true,
        feature_rssi_export: false,
        whitelisted_ssis: None,
    });
    let mut test = ComponentTest::from_config(cfg, Some(dltime));
    test.populate_entities(
        vec![TetraEntity::Cmce],
        vec![TetraEntity::Mle, TetraEntity::Umac, TetraEntity::Brew],
    );

    register_subscriber(&mut test, TEST_ISSI, TEST_GSSI);

    test.submit_message(SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Brew,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::CmceCallControl(CallControl::NetworkCallStart {
            brew_uuid: uuid::Uuid::from_u128(0x9101),
            source_issi: TEST_ISSI_2,
            dest_gssi: TEST_GSSI,
            priority: 0,
        }),
    });
    test.run_stack(Some(2));
    let _ = test.dump_sinks();

    test.submit_message(SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Brew,
        dest: TetraEntity::Cmce,
        msg: SapMsgInner::CmceCallControl(CallControl::NetworkCallStart {
            brew_uuid: uuid::Uuid::from_u128(0x9102),
            source_issi: TEST_ISSI_3,
            dest_gssi: TEST_GSSI,
            priority: 0,
        }),
    });
    test.run_stack(Some(2));
    let msgs = test.dump_sinks();
    let setups = parsed_d_setups(&msgs);
    let setup_prims: Vec<_> = msgs
        .iter()
        .filter_map(|msg| match &msg.msg {
            SapMsgInner::LcmcMleUnitdataReq(prim) if prim.chan_alloc.as_ref().is_some_and(|ca| ca.usage.is_some()) => Some(prim),
            _ => None,
        })
        .collect();

    assert!(
        setup_prims
            .iter()
            .any(|prim| prim.main_address.ssi == TEST_GSSI && !prim.stealing_permission && prim.sdu.get_len() <= 64),
        "network speaker-change late-entry D-SETUP must be compact and repeated on MCCH for scan listeners"
    );
    assert!(
        setups
            .iter()
            .any(|setup| setup.calling_party_address_ssi.is_none() && setup.facility.is_none()),
        "late-entry D-SETUP should omit optional identity fields for fast MCCH scan acquisition"
    );
    assert!(
        parsed_d_tx_granted_for_address(&msgs, TEST_GSSI).iter().any(|grant| {
            grant.call_identifier == setups[0].call_identifier
                && grant.transmitting_party_address_ssi == Some(TEST_ISSI_3 as u64)
                && grant.facility.is_none()
        }),
        "FACCH D-TX-GRANTED must keep speaker SSI but omit optional SS-TPI so it fits STCH"
    );
}

#[test]
fn test_hangtime_late_entry_dsetup_does_not_advertise_not_granted() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(
        vec![TetraEntity::Cmce],
        vec![TetraEntity::Mle, TetraEntity::Umac, TetraEntity::Brew],
    );

    register_subscriber(&mut test, TEST_ISSI, TEST_GSSI);
    test.submit_message(build_u_setup_msg(TEST_ISSI, TEST_GSSI));
    test.run_stack(Some(4));

    let mut initial_msgs = test.dump_sinks();
    for reporter in extract_d_setup_reporters(&mut initial_msgs) {
        reporter.mark_transmitted();
    }
    let call_id = parsed_d_setups(&initial_msgs)
        .first()
        .map(|setup| setup.call_identifier)
        .expect("expected initial D-SETUP");

    test.submit_message(build_u_tx_ceased_msg(TEST_ISSI, call_id));
    test.run_stack(Some(8));
    let mut hangtime_msgs = test.dump_sinks();
    for reporter in extract_d_setup_reporters(&mut hangtime_msgs) {
        reporter.mark_transmitted();
    }

    test.run_stack(Some(400));
    let late_entry_setups: Vec<_> = parsed_d_setups(&test.dump_sinks())
        .into_iter()
        .filter(|setup| setup.call_identifier == call_id)
        .collect();

    assert!(!late_entry_setups.is_empty(), "expected late-entry D-SETUP during hangtime");
    assert!(
        late_entry_setups
            .iter()
            .all(|setup| setup.transmission_grant == TransmissionGrant::GrantedToOtherUser),
        "hangtime late-entry D-SETUP must keep radios in listener/request-capable state"
    );
    assert!(
        late_entry_setups.iter().all(|setup| !setup.transmission_request_permission),
        "hangtime late-entry D-SETUP must allow transmission requests"
    );
}

#[test]
fn test_group_setup_rejects_second_active_call_for_same_gssi() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(
        vec![TetraEntity::Cmce],
        vec![TetraEntity::Mle, TetraEntity::Umac, TetraEntity::Brew],
    );

    register_subscriber(&mut test, TEST_ISSI, TEST_GSSI);
    register_subscriber(&mut test, TEST_ISSI_2, TEST_GSSI);

    test.submit_message(build_u_setup_msg(TEST_ISSI, TEST_GSSI));
    test.run_stack(Some(2));
    test.dump_sinks();

    test.submit_message(build_u_setup_msg(TEST_ISSI_2, TEST_GSSI));
    test.run_stack(Some(2));
    let msgs = test.dump_sinks();
    let releases = parsed_d_releases(&msgs);

    assert!(
        releases
            .iter()
            .any(|release| release.disconnect_cause == DisconnectCause::CalledPartyBusy),
        "second active/hangtime group call for same GSSI must be rejected as busy"
    );
}

#[test]
fn test_group_release_from_non_owner_non_holder_does_not_end_call() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(
        vec![TetraEntity::Cmce],
        vec![TetraEntity::Mle, TetraEntity::Umac, TetraEntity::Brew],
    );

    register_subscriber(&mut test, TEST_ISSI, TEST_GSSI);
    register_subscriber(&mut test, TEST_ISSI_2, TEST_GSSI);

    test.submit_message(build_u_setup_msg(TEST_ISSI, TEST_GSSI));
    test.run_stack(Some(2));
    let call_id = parsed_d_setups(&test.dump_sinks())
        .first()
        .map(|setup| setup.call_identifier)
        .expect("expected group D-SETUP");

    test.submit_message(build_u_release_msg(TEST_ISSI_2, call_id));
    test.run_stack(Some(2));
    test.dump_sinks();

    test.submit_message(build_u_tx_ceased_msg(TEST_ISSI, call_id));
    test.run_stack(Some(2));
    let msgs = test.dump_sinks();

    assert!(
        parsed_d_tx_ceased(&msgs).iter().any(|ceased| ceased.call_identifier == call_id),
        "call should still be active after unauthorized U-RELEASE"
    );
}

#[test]
fn test_individual_simplex_floor_requires_free_floor_and_holder_ceased() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime { h: 0, m: 1, f: 1, t: 1 };
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(
        vec![TetraEntity::Cmce],
        vec![TetraEntity::Mle, TetraEntity::Umac, TetraEntity::Brew],
    );

    register_subscriber(&mut test, TEST_ISSI, TEST_GSSI);
    register_subscriber(&mut test, TEST_ISSI_2, TEST_GSSI);
    register_subscriber(&mut test, TEST_ISSI_3, TEST_GSSI);

    test.submit_message(build_u_setup_p2p_msg(TEST_ISSI, TEST_ISSI_2, false, 0));
    test.run_stack(Some(2));
    let call_id = parsed_d_setups_any(&test.dump_sinks())
        .first()
        .map(|setup| setup.call_identifier)
        .expect("expected individual D-SETUP");

    test.submit_message(build_u_connect_msg(TEST_ISSI_2, call_id, false));
    test.run_stack(Some(2));
    test.dump_sinks();

    test.submit_message(build_u_tx_demand_msg(TEST_ISSI, call_id, 0));
    test.run_stack(Some(2));
    let denied_msgs = test.dump_sinks();
    assert!(
        parsed_d_tx_granted_to(&denied_msgs, TEST_ISSI).iter().any(|grant| {
            grant.call_identifier == call_id && grant.transmission_grant == TransmissionGrant::NotGranted.into_raw() as u8
        }),
        "caller must be denied while called party holds initial simplex floor"
    );

    test.submit_message(build_u_tx_ceased_msg(TEST_ISSI, call_id));
    test.run_stack(Some(2));
    let non_holder_ceased_msgs = test.dump_sinks();
    assert!(
        parsed_d_tx_ceased(&non_holder_ceased_msgs).is_empty(),
        "U-TX-CEASED from non-holder must not emit floor PDUs"
    );

    test.submit_message(build_u_tx_ceased_msg(TEST_ISSI_2, call_id));
    test.run_stack(Some(2));
    let holder_ceased_msgs = test.dump_sinks();
    assert!(
        parsed_d_tx_ceased(&holder_ceased_msgs)
            .iter()
            .any(|ceased| ceased.call_identifier == call_id),
        "current holder U-TX-CEASED must release simplex floor"
    );

    test.submit_message(build_u_tx_demand_msg(TEST_ISSI, call_id, 0));
    test.run_stack(Some(2));
    let granted_msgs = test.dump_sinks();
    assert!(
        parsed_d_tx_granted_to(&granted_msgs, TEST_ISSI)
            .iter()
            .any(|grant| { grant.call_identifier == call_id && grant.transmission_grant == TransmissionGrant::Granted.into_raw() as u8 }),
        "caller must get floor after holder releases it"
    );

    test.submit_message(build_u_tx_demand_msg(TEST_ISSI_3, call_id, 0));
    test.run_stack(Some(2));
    let outsider_msgs = test.dump_sinks();
    assert!(
        parsed_d_tx_granted_to(&outsider_msgs, TEST_ISSI_3).is_empty(),
        "non-participant U-TX-DEMAND must be ignored"
    );
}
