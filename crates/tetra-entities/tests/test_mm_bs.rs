mod common;

use tetra_config::bluestation::StackMode;
use tetra_core::tetra_entities::TetraEntity;
use tetra_core::{BitBuffer, Sap, SsiType, TdmaTime, TetraAddress, debug};
use tetra_pdus::mm::enums::energy_saving_mode::EnergySavingMode;
use tetra_pdus::mm::enums::location_update_type::LocationUpdateType;
use tetra_pdus::mm::enums::mm_pdu_type_dl::MmPduTypeDl;
use tetra_pdus::mm::fields::group_identity_location_demand::GroupIdentityLocationDemand;
use tetra_pdus::mm::fields::group_identity_uplink::GroupIdentityUplink;
use tetra_pdus::mm::pdus::d_location_update_accept::DLocationUpdateAccept;
use tetra_pdus::mm::pdus::d_mm_status::DMmStatus;
use tetra_pdus::mm::pdus::u_attach_detach_group_identity::UAttachDetachGroupIdentity;
use tetra_pdus::mm::pdus::u_location_update_demand::ULocationUpdateDemand;
use tetra_saps::control::brew::{BrewSubscriberAction, MmSubscriberUpdate};
use tetra_saps::lmm::LmmMleUnitdataInd;
use tetra_saps::sapmsg::{SapMsg, SapMsgInner};

use crate::common::ComponentTest;

const TEST_ISSI: u32 = 2260082;

fn make_location_update_msg(issi: u32, handle: u32, location_update_type: LocationUpdateType) -> SapMsg {
    make_location_update_msg_with_group_demand(issi, handle, location_update_type, None)
}

fn make_location_update_msg_with_group_demand(
    issi: u32,
    handle: u32,
    location_update_type: LocationUpdateType,
    group_identity_location_demand: Option<GroupIdentityLocationDemand>,
) -> SapMsg {
    let pdu = ULocationUpdateDemand {
        location_update_type,
        request_to_append_la: false,
        cipher_control: false,
        ciphering_parameters: None,
        class_of_ms: None,
        energy_saving_mode: None,
        la_information: None,
        ssi: None,
        address_extension: None,
        group_identity_location_demand,
        group_report_response: None,
        authentication_uplink: None,
        extended_capabilities: None,
        proprietary: None,
    };
    let mut sdu = BitBuffer::new_autoexpand(16);
    pdu.to_bitbuf(&mut sdu).unwrap();
    sdu.seek(0);

    SapMsg {
        sap: Sap::LmmSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Mm,
        msg: SapMsgInner::LmmMleUnitdataInd(LmmMleUnitdataInd {
            sdu,
            handle,
            received_address: TetraAddress::issi(issi),
        }),
    }
}

fn make_attach_group_msg(issi: u32, handle: u32, gssi: u32) -> SapMsg {
    let pdu = UAttachDetachGroupIdentity {
        group_identity_report: false,
        group_identity_attach_detach_mode: false,
        group_report_response: None,
        group_identity_uplink: Some(vec![GroupIdentityUplink {
            class_of_usage: Some(0),
            group_identity_detachment_uplink: None,
            gssi: Some(gssi),
            address_extension: None,
            vgssi: None,
        }]),
        proprietary: None,
    };
    let mut sdu = BitBuffer::new_autoexpand(32);
    pdu.to_bitbuf(&mut sdu).unwrap();
    sdu.seek(0);

    SapMsg {
        sap: Sap::LmmSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Mm,
        msg: SapMsgInner::LmmMleUnitdataInd(LmmMleUnitdataInd {
            sdu,
            handle,
            received_address: TetraAddress::issi(issi),
        }),
    }
}

fn subscriber_updates(msgs: &[SapMsg]) -> Vec<MmSubscriberUpdate> {
    msgs.iter()
        .filter_map(|msg| {
            let SapMsgInner::MmSubscriberUpdate(update) = &msg.msg else {
                return None;
            };
            Some(update.clone())
        })
        .collect()
}

fn lmm_downlink_pdu_types(msgs: &[SapMsg]) -> Vec<MmPduTypeDl> {
    msgs.iter()
        .filter_map(|msg| {
            let SapMsgInner::LmmMleUnitdataReq(ref prim) = msg.msg else {
                return None;
            };
            let mut sdu = BitBuffer::from_bitstr(&prim.sdu.to_bitstr());
            Some(MmPduTypeDl::try_from(sdu.read_field(4, "pdu_type").unwrap()).unwrap())
        })
        .collect()
}

fn first_location_update_accept(msgs: &[SapMsg]) -> DLocationUpdateAccept {
    let response = msgs
        .iter()
        .find_map(|msg| {
            if let SapMsgInner::LmmMleUnitdataReq(ref prim) = msg.msg {
                Some(prim)
            } else {
                None
            }
        })
        .expect("expected D-LOCATION UPDATE ACCEPT");
    let mut resp_sdu = BitBuffer::from_bitstr(&response.sdu.to_bitstr());
    DLocationUpdateAccept::from_bitbuf(&mut resp_sdu).expect("failed parsing D-LOCATION UPDATE ACCEPT")
}

#[test]
fn test_u_mm_status_energy_saving() {
    // Motorola requesting power management (ChangeOfEnergySavingModeRequest)
    debug::setup_logging_verbose();
    let test_vec1 = "00110000010010";
    let dltime_vec1 = TdmaTime::default().add_timeslots(2); // Downlink time: 0/1/1/3
    // let ultime_vec1 = dltime_vec1.add_timeslots(-2); // Uplink time: 0/1/1/1
    let test_prim1 = LmmMleUnitdataInd {
        sdu: BitBuffer::from_bitstr(test_vec1),
        handle: 0,
        received_address: TetraAddress {
            ssi_type: SsiType::Issi,
            ssi: 2040814,
        },
    };
    let test_sapmsg1 = SapMsg {
        sap: Sap::LmmSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Mm,
        msg: SapMsgInner::LmmMleUnitdataInd(test_prim1),
    };

    // Setup testing stack
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime_vec1));
    let components = vec![TetraEntity::Mm];
    let sinks: Vec<TetraEntity> = vec![TetraEntity::Mle];
    test.populate_entities(components, sinks);

    // Submit and process message
    test.submit_message(test_sapmsg1);
    test.run_stack(Some(1));
    let sink_msgs = test.dump_sinks();

    // FlowStation explicitly allocates StayAlive until addressed downlink EE
    // scheduling is complete.
    assert_eq!(sink_msgs.len(), 1);

    // Parse the response and verify it's a D-MM-STATUS
    let SapMsgInner::LmmMleUnitdataReq(ref resp_prim) = sink_msgs[0].msg else {
        panic!("Expected LmmMleUnitdataReq");
    };
    let mut resp_sdu = BitBuffer::from_bitstr(&resp_prim.sdu.to_bitstr());
    let resp_pdu = DMmStatus::from_bitbuf(&mut resp_sdu).expect("Failed parsing D-MM-STATUS response");
    assert_eq!(
        resp_pdu.status_downlink,
        tetra_pdus::mm::enums::status_downlink::StatusDownlink::ChangeOfEnergySavingModeResponse
    );
    let esi = resp_pdu.energy_saving_information.expect("expected energy saving information");
    assert_eq!(esi.energy_saving_mode, EnergySavingMode::StayAlive);
    assert!(esi.frame_number.is_none());
    assert!(esi.multiframe_number.is_none());
}

#[test]
fn test_itsi_attach_emits_only_location_update_accept_no_automatic_command() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let sink_msgs = test.dump_sinks();

    assert_eq!(lmm_downlink_pdu_types(&sink_msgs), vec![MmPduTypeDl::DLocationUpdateAccept]);

    let resp_pdu = first_location_update_accept(&sink_msgs);
    assert_eq!(resp_pdu.location_update_accept_type, LocationUpdateType::ItsiAttach);
    assert_eq!(resp_pdu.ssi, Some(TEST_ISSI as u64));
}

#[test]
fn test_force_location_update_keeps_explicit_command_but_no_second_automatic_command() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle]);

    test.submit_message(SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Cmce,
        dest: TetraEntity::Mm,
        msg: SapMsgInner::MmForceLocationUpdate {
            issi: TEST_ISSI,
            handle: 17,
        },
    });
    test.run_stack(Some(1));
    let command_msgs = test.dump_sinks();
    assert_eq!(lmm_downlink_pdu_types(&command_msgs), vec![MmPduTypeDl::DLocationUpdateCommand]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let response_msgs = test.dump_sinks();
    assert_eq!(lmm_downlink_pdu_types(&response_msgs), vec![MmPduTypeDl::DLocationUpdateAccept]);
}

#[test]
fn test_brew_reconnected_emits_location_update_command_for_registered_ms() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();

    test.submit_message(SapMsg {
        sap: Sap::Control,
        src: TetraEntity::Brew,
        dest: TetraEntity::Mm,
        msg: SapMsgInner::BrewReconnected,
    });
    test.run_stack(Some(1));
    let command_msgs = test.dump_sinks();
    assert_eq!(lmm_downlink_pdu_types(&command_msgs), vec![MmPduTypeDl::DLocationUpdateCommand]);
}

#[test]
fn test_roaming_location_update_preserves_group_affiliation_without_group_replace() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle, TetraEntity::Cmce]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();

    test.submit_message(make_attach_group_msg(TEST_ISSI, 17, 91));
    test.run_stack(Some(1));
    let attach_msgs = test.dump_sinks();
    assert!(
        subscriber_updates(&attach_msgs)
            .iter()
            .any(|update| update.issi == TEST_ISSI && update.action == BrewSubscriberAction::Affiliate && update.groups == vec![91]),
        "initial group attach must affiliate GSSI 91"
    );

    test.submit_message(make_location_update_msg(TEST_ISSI, 18, LocationUpdateType::RoamingLocationUpdating));
    test.run_stack(Some(1));
    let refresh_msgs = test.dump_sinks();
    let updates = subscriber_updates(&refresh_msgs);

    assert!(
        !updates.iter().any(|update| update.issi == TEST_ISSI
            && matches!(update.action, BrewSubscriberAction::Deregister | BrewSubscriberAction::Deaffiliate)),
        "roaming LU without a valid group replace must not transiently remove TG91"
    );
    assert!(
        updates
            .iter()
            .any(|update| update.issi == TEST_ISSI && update.action == BrewSubscriberAction::Register && update.groups.is_empty()),
        "roaming LU should refresh registration without disturbing current affiliations"
    );
    assert!(test.config.state_read().subscribers.has_group_members(91));
}

#[test]
fn test_roaming_location_update_mode1_without_uplink_rejects_without_losing_affiliation() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle, TetraEntity::Cmce]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();

    test.submit_message(make_attach_group_msg(TEST_ISSI, 17, 91));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();
    assert!(test.config.state_read().subscribers.has_group_members(91));

    test.submit_message(make_location_update_msg_with_group_demand(
        TEST_ISSI,
        18,
        LocationUpdateType::RoamingLocationUpdating,
        Some(GroupIdentityLocationDemand {
            group_identity_attach_detach_mode: 1,
            group_identity_uplink: None,
        }),
    ));
    test.run_stack(Some(1));
    let refresh_msgs = test.dump_sinks();
    let updates = subscriber_updates(&refresh_msgs);

    assert!(
        !updates
            .iter()
            .any(|update| update.issi == TEST_ISSI && update.action == BrewSubscriberAction::Deaffiliate && update.groups == vec![91]),
        "mode=1 without GroupIdentityUplink is unsupported and must not detach the prior GSSI"
    );
    assert!(test.config.state_read().subscribers.has_group_members(91));
}

#[test]
fn test_roaming_group_replace_with_unsupported_identity_rejects_without_losing_tg91() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle, TetraEntity::Cmce]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();

    test.submit_message(make_attach_group_msg(TEST_ISSI, 17, 91));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();
    assert!(test.config.state_read().subscribers.has_group_members(91));

    test.submit_message(make_location_update_msg_with_group_demand(
        TEST_ISSI,
        18,
        LocationUpdateType::RoamingLocationUpdating,
        Some(GroupIdentityLocationDemand {
            group_identity_attach_detach_mode: 1,
            group_identity_uplink: Some(vec![GroupIdentityUplink {
                class_of_usage: Some(0),
                group_identity_detachment_uplink: None,
                gssi: Some(91),
                address_extension: Some(901999),
                vgssi: None,
            }]),
        }),
    ));
    test.run_stack(Some(1));
    let refresh_msgs = test.dump_sinks();
    let updates = subscriber_updates(&refresh_msgs);
    let accept = first_location_update_accept(&refresh_msgs);
    let gila = accept
        .group_identity_location_accept
        .expect("expected explicit group identity response");

    assert_eq!(
        gila.group_identity_accept_reject, 1,
        "unsupported group identity replace must be rejected"
    );
    assert!(
        !updates.iter().any(|update| update.issi == TEST_ISSI
            && matches!(update.action, BrewSubscriberAction::Deregister | BrewSubscriberAction::Deaffiliate)),
        "roaming cleanup must not remove TG91 after rejecting unsupported replacement"
    );
    assert!(test.config.state_read().subscribers.has_group_members(91));
}

#[test]
fn test_roaming_group_replace_with_mixed_identity_list_rejects_without_losing_tg91() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle, TetraEntity::Cmce]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();

    test.submit_message(make_attach_group_msg(TEST_ISSI, 17, 91));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();
    assert!(test.config.state_read().subscribers.has_group_members(91));

    test.submit_message(make_location_update_msg_with_group_demand(
        TEST_ISSI,
        18,
        LocationUpdateType::RoamingLocationUpdating,
        Some(GroupIdentityLocationDemand {
            group_identity_attach_detach_mode: 1,
            group_identity_uplink: Some(vec![
                GroupIdentityUplink {
                    class_of_usage: Some(0),
                    group_identity_detachment_uplink: None,
                    gssi: Some(226777),
                    address_extension: None,
                    vgssi: None,
                },
                GroupIdentityUplink {
                    class_of_usage: Some(0),
                    group_identity_detachment_uplink: None,
                    gssi: Some(91),
                    address_extension: Some(901999),
                    vgssi: None,
                },
            ]),
        }),
    ));
    test.run_stack(Some(1));
    let refresh_msgs = test.dump_sinks();
    let updates = subscriber_updates(&refresh_msgs);
    let accept = first_location_update_accept(&refresh_msgs);
    let gila = accept
        .group_identity_location_accept
        .expect("expected explicit group identity response");

    assert_eq!(
        gila.group_identity_accept_reject, 1,
        "mixed supported/unsupported group replace must be rejected atomically"
    );
    assert!(
        !updates.iter().any(|update| update.issi == TEST_ISSI
            && matches!(update.action, BrewSubscriberAction::Deregister | BrewSubscriberAction::Deaffiliate)),
        "mixed unsupported replace must not transiently remove TG91"
    );
    assert!(test.config.state_read().subscribers.has_group_members(91));
}

#[test]
fn test_known_itsi_attach_restores_prior_group_affiliation() {
    debug::setup_logging_verbose();

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle, TetraEntity::Cmce]);

    test.submit_message(make_location_update_msg(TEST_ISSI, 17, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();

    test.submit_message(make_attach_group_msg(TEST_ISSI, 17, 91));
    test.run_stack(Some(1));
    let _ = test.dump_sinks();
    assert!(test.config.state_read().subscribers.has_group_members(91));

    test.submit_message(make_location_update_msg(TEST_ISSI, 18, LocationUpdateType::ItsiAttach));
    test.run_stack(Some(1));
    let reattach_msgs = test.dump_sinks();
    let updates = subscriber_updates(&reattach_msgs);

    assert!(
        updates
            .iter()
            .any(|update| update.issi == TEST_ISSI && update.action == BrewSubscriberAction::Register && update.groups.is_empty()),
        "known ItsiAttach should still refresh Brew/CMCE registration"
    );
    assert!(
        updates
            .iter()
            .any(|update| update.issi == TEST_ISSI && update.action == BrewSubscriberAction::Affiliate && update.groups == vec![91]),
        "known ItsiAttach must restore prior TG91 affiliation after registry re-register"
    );
    assert!(test.config.state_read().subscribers.has_group_members(91));
}

#[test]
fn test_location_update_accept_preserves_request_type_when_periodic_enabled() {
    debug::setup_logging_verbose();

    let lud_with_eg1 =
        "0010000001100010010010100000010000010010001001100000111000001110000000010010000000101000000000000000000000001101000";
    let test_prim = LmmMleUnitdataInd {
        sdu: BitBuffer::from_bitstr(lud_with_eg1),
        handle: 0,
        received_address: TetraAddress {
            ssi_type: SsiType::Issi,
            ssi: 2260616,
        },
    };
    let test_sapmsg = SapMsg {
        sap: Sap::LmmSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Mm,
        msg: SapMsgInner::LmmMleUnitdataInd(test_prim),
    };

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut cfg = ComponentTest::get_default_test_config(StackMode::Bs);
    cfg.cell.periodic_registration_secs = 3600;
    let mut test = ComponentTest::from_config(cfg, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle]);

    test.submit_message(test_sapmsg);
    test.run_stack(Some(1));
    let sink_msgs = test.dump_sinks();

    let response = sink_msgs
        .iter()
        .find_map(|msg| {
            if let SapMsgInner::LmmMleUnitdataReq(ref prim) = msg.msg {
                Some(prim)
            } else {
                None
            }
        })
        .expect("expected D-LOCATION UPDATE ACCEPT");

    let mut resp_sdu = BitBuffer::from_bitstr(&response.sdu.to_bitstr());
    let resp_pdu = DLocationUpdateAccept::from_bitbuf(&mut resp_sdu).expect("failed parsing D-LOCATION UPDATE ACCEPT");
    assert_eq!(resp_pdu.location_update_accept_type, LocationUpdateType::RoamingLocationUpdating);
}

#[test]
fn test_location_update_energy_saving_request_is_forced_to_stay_alive_until_ee_scheduler_exists() {
    debug::setup_logging_verbose();

    let lud_with_eg1 =
        "0010000001100010010010100000010000010010001001100000111000001110000000010010000000101000000000000000000000001101000";
    let test_prim = LmmMleUnitdataInd {
        sdu: BitBuffer::from_bitstr(lud_with_eg1),
        handle: 0,
        received_address: TetraAddress {
            ssi_type: SsiType::Issi,
            ssi: 2260616,
        },
    };
    let test_sapmsg = SapMsg {
        sap: Sap::LmmSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Mm,
        msg: SapMsgInner::LmmMleUnitdataInd(test_prim),
    };

    let dltime = TdmaTime::default().add_timeslots(2);
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime));
    test.populate_entities(vec![TetraEntity::Mm], vec![TetraEntity::Mle, TetraEntity::Umac]);

    test.submit_message(test_sapmsg);
    test.run_stack(Some(1));
    let sink_msgs = test.dump_sinks();

    let response = sink_msgs
        .iter()
        .find_map(|msg| {
            if let SapMsgInner::LmmMleUnitdataReq(ref prim) = msg.msg {
                Some(prim)
            } else {
                None
            }
        })
        .expect("expected D-LOCATION UPDATE ACCEPT");

    let mut resp_sdu = BitBuffer::from_bitstr(&response.sdu.to_bitstr());
    let resp_pdu = DLocationUpdateAccept::from_bitbuf(&mut resp_sdu).expect("failed parsing D-LOCATION UPDATE ACCEPT");
    let esi = resp_pdu
        .energy_saving_information
        .expect("expected explicit StayAlive while real EE scheduling is disabled");
    assert_eq!(esi.energy_saving_mode, EnergySavingMode::StayAlive);
    assert!(esi.frame_number.is_none());
    assert!(esi.multiframe_number.is_none());

    let umac_update = sink_msgs
        .iter()
        .find_map(|msg| {
            if let SapMsgInner::MmEnergySavingUpdate { issi, mode, start_time } = msg.msg {
                Some((issi, mode, start_time))
            } else {
                None
            }
        })
        .expect("expected MM energy-saving update to UMAC");

    assert_eq!(umac_update.0, 2260616);
    assert_eq!(umac_update.1, EnergySavingMode::StayAlive as u8);
    assert!(umac_update.2.is_none());
}
