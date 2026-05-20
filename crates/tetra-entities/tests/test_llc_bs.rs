mod common;

use common::ComponentTest;
use tetra_config::bluestation::StackMode;
use tetra_core::tetra_entities::TetraEntity;
use tetra_core::{BitBuffer, Sap, SsiType, TdmaTime, TetraAddress, TxReporter, TxState, debug};
use tetra_pdus::llc::pdus::bl_ack::BlAck;
use tetra_pdus::llc::pdus::bl_data::BlData;
use tetra_saps::sapmsg::{SapMsg, SapMsgInner};
use tetra_saps::tla::TlaTlDataReqBl;
use tetra_saps::tma::TmaUnitdataInd;

#[test]
fn test_udata_with_broken_mm_payload() {
    // INCOMPLETE VECTOR replace with something more meaningful
    debug::setup_logging_verbose();

    // FIXME make proper vec here that can be passed onwards
    let test_vec = "00011001011100111000000011111100001000010000000000000000"; // INCOMPLETE
    let dltime_vec = TdmaTime::default().add_timeslots(2); // Downlink time: 0/1/1/3
    let test_prim = TmaUnitdataInd {
        pdu: Some(BitBuffer::from_bitstr(test_vec)),
        main_address: TetraAddress {
            ssi: 2065022,
            ssi_type: SsiType::Issi,
        },
        scrambling_code: 864282631,
        time: None,
        endpoint_id: 0,
        new_endpoint_id: None,
        css_endpoint_id: None,
        air_interface_encryption: 0,
        chan_change_response_req: false,
        chan_change_handle: None,
        chan_info: None,
    };
    let test_sapmsg = SapMsg {
        sap: Sap::TmaSap,
        src: TetraEntity::Umac,
        dest: TetraEntity::Llc,
        msg: SapMsgInner::TmaUnitdataInd(test_prim),
    };

    // Setup testing stack
    let mut test = ComponentTest::new(StackMode::Bs, Some(dltime_vec));
    let components = vec![TetraEntity::Llc, TetraEntity::Mle, TetraEntity::Mm];
    let sinks: Vec<TetraEntity> = vec![TetraEntity::Umac];
    test.populate_entities(components, sinks);

    // Submit and process message
    test.submit_message(test_sapmsg);
    test.run_stack(Some(1));
    let sink_msgs = test.dump_sinks();

    // Evaluate results
    assert_eq!(sink_msgs.len(), 1);
    tracing::warn!("Validation of result not implemented");
}

#[test]
fn test_unsupported_acked_stch_marks_reporter_discarded() {
    debug::setup_logging_verbose();

    let reporter = TxReporter::new();
    let receipt = reporter.clone();
    let test_sapmsg = SapMsg {
        sap: Sap::TlaSap,
        src: TetraEntity::Mle,
        dest: TetraEntity::Llc,
        msg: SapMsgInner::TlaTlDataReqBl(TlaTlDataReqBl {
            main_address: TetraAddress::issi(2260082),
            link_id: 0,
            endpoint_id: 0,
            tl_sdu: BitBuffer::from_bitstr("10101010"),
            stealing_permission: true,
            subscriber_class: 0,
            fcs_flag: false,
            air_interface_encryption: None,
            stealing_repeats_flag: None,
            data_class_info: None,
            req_handle: 0,
            graceful_degradation: None,
            chan_alloc: None,
            tx_reporter: Some(reporter),
        }),
    };

    let mut test = ComponentTest::new(StackMode::Bs, None);
    test.populate_entities(vec![TetraEntity::Llc], vec![TetraEntity::Umac]);
    test.submit_message(test_sapmsg);
    test.run_stack(Some(1));

    assert_eq!(receipt.get_state(), TxState::Discarded);
    assert!(test.dump_sinks().is_empty());
}

#[test]
fn test_bl_ack_uses_propagated_rx_timeslot() {
    debug::setup_logging_verbose();

    let rx_time = TdmaTime::default().add_timeslots(5); // ts2
    let mut llc_pdu = BitBuffer::new_autoexpand(8);
    BlData { has_fcs: false, ns: 1 }.to_bitbuf(&mut llc_pdu);
    llc_pdu.seek(0);

    let test_sapmsg = SapMsg {
        sap: Sap::TmaSap,
        src: TetraEntity::Umac,
        dest: TetraEntity::Llc,
        msg: SapMsgInner::TmaUnitdataInd(TmaUnitdataInd {
            pdu: Some(llc_pdu),
            main_address: TetraAddress::issi(2260618),
            scrambling_code: 864282631,
            time: Some(rx_time),
            endpoint_id: 0,
            new_endpoint_id: None,
            css_endpoint_id: None,
            air_interface_encryption: 0,
            chan_change_response_req: false,
            chan_change_handle: None,
            chan_info: None,
        }),
    };

    let mut test = ComponentTest::new(StackMode::Bs, Some(TdmaTime::default())); // local LLC tick is not rx_time
    test.populate_entities(vec![TetraEntity::Llc], vec![TetraEntity::Umac]);
    test.submit_message(test_sapmsg);
    test.run_stack(Some(1));

    let sink_msgs = test.dump_sinks();
    let ack_req = sink_msgs
        .iter()
        .find_map(|msg| match &msg.msg {
            SapMsgInner::TmaUnitdataReq(prim) => Some(prim),
            _ => None,
        })
        .expect("expected LLC BL-ACK to UMAC");

    assert!(ack_req.stealing_permission, "traffic-channel uplink must be ACKed via stealing");
    assert_eq!(
        ack_req
            .chan_alloc
            .as_ref()
            .expect("traffic-channel ACK should carry channel allocation")
            .timeslots,
        [false, true, false, false],
        "BL-ACK must target the propagated RX timeslot, not the current LLC tick"
    );

    let mut ack_bits = BitBuffer::from_bitstr(&ack_req.pdu.to_bitstr());
    let ack = BlAck::from_bitbuf(&mut ack_bits).expect("expected BL-ACK PDU");
    assert_eq!(ack.nr, 1);
}
