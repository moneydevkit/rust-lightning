#![cfg(all(test, feature = "std", feature = "time"))]

mod common;

use common::{
	create_service_client_and_payer_nodes, get_lsps_message, LSPSNodesWithPayer, LiquidityNode,
};

use lightning::check_added_monitors;
use lightning::events::Event;
use lightning::ln::channelmanager::RecipientOnionFields;
use lightning::ln::channelmanager::{InterceptId, PaymentId};
use lightning::ln::functional_test_utils::{
	create_chan_between_nodes_with_value, create_chanmon_cfgs, create_network, create_node_cfgs,
	create_node_chanmgrs, do_commitment_signed_dance, test_default_channel_config, Node, SendEvent,
};
use lightning::ln::msgs::{BaseMessageHandler, ChannelMessageHandler};
use lightning::ln::onion_utils::create_payment_onion;
use lightning::ln::peer_handler::CustomMessageHandler;
use lightning::routing::router::{Path, Route, RouteHop};
use lightning::types::features::ChannelFeatures;

use lightning_liquidity::events::LiquidityEvent;
use lightning_liquidity::lsps4::client::LSPS4ClientConfig;
use lightning_liquidity::lsps4::event::{LSPS4ClientEvent, LSPS4ServiceEvent};
use lightning_liquidity::lsps4::service::LSPS4ServiceConfig;
use lightning_liquidity::utils::time::DefaultTimeProvider;
use lightning_liquidity::{LiquidityClientConfig, LiquidityServiceConfig};

use lightning_types::payment::{PaymentHash, PaymentSecret};

use bitcoin::secp256k1::{Secp256k1, SecretKey};

use std::sync::Arc;

const AMOUNT_MSAT: u64 = 10_000_000;
const CLTV_DELTA: u32 = 144;

struct Intercepted {
	id: InterceptId,
	payment_hash: PaymentHash,
	expected_outbound_amount_msat: u64,
}

fn register(service_node: &LiquidityNode, client_node: &LiquidityNode) -> u64 {
	let service_node_id = service_node.inner.node.get_our_node_id();
	let client_node_id = client_node.inner.node.get_our_node_id();
	let client_handler = client_node.liquidity_manager.lsps4_client_handler().unwrap();

	client_handler.register_node(service_node_id, None).unwrap();
	let request = get_lsps_message!(client_node, service_node_id);
	service_node.liquidity_manager.handle_custom_message(request, client_node_id).unwrap();
	let response = get_lsps_message!(service_node, client_node_id);
	client_node.liquidity_manager.handle_custom_message(response, service_node_id).unwrap();

	match client_node.liquidity_manager.next_event().unwrap() {
		LiquidityEvent::LSPS4Client(LSPS4ClientEvent::InvoiceParametersReady {
			intercept_scid,
			..
		}) => intercept_scid,
		e => panic!("Unexpected event: {:?}", e),
	}
}

fn run(test: impl FnOnce(&LSPSNodesWithPayer, u64)) {
	let chanmon_cfgs = create_chanmon_cfgs(3);
	let node_cfgs = create_node_cfgs(3, &chanmon_cfgs);
	let mut lsp_config = test_default_channel_config();
	lsp_config.accept_intercept_htlcs = true;
	let node_chanmgrs = create_node_chanmgrs(3, &node_cfgs, &[Some(lsp_config), None, None]);
	let nodes = create_network(3, &node_cfgs, &node_chanmgrs);

	let service_config = LiquidityServiceConfig {
		#[cfg(lsps1_service)]
		lsps1_service_config: None,
		lsps2_service_config: None,
		lsps4_service_config: Some(LSPS4ServiceConfig {
			cltv_expiry_delta: CLTV_DELTA,
			forwarding_fee_proportional_millionths: 20_000,
			issuer_pubkeys: vec![],
		}),
		lsps5_service_config: None,
		advertise_service: true,
	};
	let client_config = LiquidityClientConfig {
		lsps1_client_config: None,
		lsps2_client_config: None,
		lsps4_client_config: Some(LSPS4ClientConfig {}),
		lsps5_client_config: None,
	};
	let lsps_nodes = create_service_client_and_payer_nodes(
		nodes,
		service_config,
		client_config,
		Arc::new(DefaultTimeProvider),
	);
	create_chan_between_nodes_with_value(
		&lsps_nodes.payer_node,
		&lsps_nodes.service_node.inner,
		1_000_000,
		0,
	);
	let intercept_scid = register(&lsps_nodes.service_node, &lsps_nodes.client_node);
	test(&lsps_nodes, intercept_scid);
}

fn path_to_client(
	nodes: &LSPSNodesWithPayer, intercept_scid: u64, lsp_fee_msat: u64, final_msat: u64,
) -> Path {
	let service = &nodes.service_node;
	let client = &nodes.client_node;
	let first_hop = &nodes.payer_node.node.list_usable_channels()[0];
	Path {
		hops: vec![
			RouteHop {
				pubkey: service.node.get_our_node_id(),
				node_features: service.node.node_features(),
				short_channel_id: first_hop.short_channel_id.unwrap(),
				channel_features: ChannelFeatures::empty(),
				fee_msat: lsp_fee_msat,
				cltv_expiry_delta: CLTV_DELTA,
				maybe_announced_channel: false,
			},
			RouteHop {
				pubkey: client.node.get_our_node_id(),
				node_features: client.node.node_features(),
				short_channel_id: intercept_scid,
				channel_features: ChannelFeatures::empty(),
				fee_msat: final_msat,
				cltv_expiry_delta: 40,
				maybe_announced_channel: false,
			},
		],
		blinded_tail: None,
	}
}

/// Sends `sent_msat` (plus `lsp_fee_msat`) from the payer to the LSP's intercept scid and returns
/// what the LSP's `HTLCIntercepted` event reports. If `onion_claims_msat` is set, the onion is
/// swapped for one that asks the LSP to forward that amount instead, which is what the
/// 2026-09-11 attacker did: a 10 sat HTLC whose onion asked for a 32k sat forward.
fn intercept_htlc(
	nodes: &LSPSNodesWithPayer, intercept_scid: u64, id: u8, lsp_fee_msat: u64, sent_msat: u64,
	onion_claims_msat: Option<u64>,
) -> Intercepted {
	let payer = &nodes.payer_node;
	let service = &nodes.service_node;
	let payment_hash = PaymentHash([id; 32]);
	let recipient_onion = RecipientOnionFields::secret_only(PaymentSecret([id; 32]));
	let path = path_to_client(nodes, intercept_scid, lsp_fee_msat, sent_msat);
	let route = Route { paths: vec![path], route_params: None };
	payer
		.node
		.send_payment_with_route(route, payment_hash, recipient_onion.clone(), PaymentId([id; 32]))
		.unwrap();
	check_added_monitors!(payer, 1);
	let mut ev = SendEvent::from_event(payer.node.get_and_clear_pending_msg_events().remove(0));

	if let Some(claimed_msat) = onion_claims_msat {
		let fake_path = path_to_client(nodes, intercept_scid, 0, claimed_msat);
		let height = payer.best_block_info().1 + 1;
		let (onion, _, _) = create_payment_onion(
			&Secp256k1::new(),
			&fake_path,
			&SecretKey::from_slice(&[id; 32]).unwrap(),
			claimed_msat,
			&recipient_onion,
			height,
			&payment_hash,
			&None,
			None,
			[id; 32],
		)
		.unwrap();
		// The HTLC amount (and so the commitment tx) stays honest, only the onion lies.
		ev.msgs[0].onion_routing_packet = onion;
	}

	service.node.handle_update_add_htlc(payer.node.get_our_node_id(), &ev.msgs[0]);
	do_commitment_signed_dance(&service.inner, payer, &ev.commitment_msg, false, true);
	service.node.process_pending_htlc_forwards();
	let events = service.node.get_and_clear_pending_events();
	assert_eq!(events.len(), 1, "{:?}", events);
	match &events[0] {
		Event::HTLCIntercepted {
			intercept_id,
			requested_next_hop_scid,
			expected_outbound_amount_msat,
			inbound_amount_msat,
			..
		} => {
			assert_eq!(*requested_next_hop_scid, intercept_scid);
			assert_eq!(*inbound_amount_msat, sent_msat + lsp_fee_msat);
			Intercepted {
				id: *intercept_id,
				payment_hash,
				expected_outbound_amount_msat: *expected_outbound_amount_msat,
			}
		},
		e => panic!("Expected HTLCIntercepted, got {:?}", e),
	}
}

/// Hands the event to the LSPS4 service the way ldk-node does.
fn handle(service_node: &LiquidityNode, scid: u64, htlc: &Intercepted) {
	service_node
		.liquidity_manager
		.lsps4_service_handler()
		.unwrap()
		.htlc_intercepted(scid, htlc.id, htlc.expected_outbound_amount_msat, htlc.payment_hash)
		.unwrap();
}

fn connect_client(nodes: &LSPSNodesWithPayer) {
	let handler = nodes.service_node.liquidity_manager.lsps4_service_handler().unwrap();
	handler.peer_connected(nodes.client_node.node.get_our_node_id());
}

fn assert_failed_back(service_node: &Node, htlc: &Intercepted) {
	assert_eq!(service_node.node.intercepted_htlc_inbound_amount_msat(htlc.id), None);
	assert!(matches!(
		service_node.node.get_and_clear_pending_events().as_slice(),
		[Event::HTLCHandlingFailed { .. }]
	));
}

#[test]
fn offline_peer_honest_htlc_is_held_and_wakes_peer() {
	run(|nodes, scid| {
		let htlc = intercept_htlc(nodes, scid, 1, 0, AMOUNT_MSAT, None);
		handle(&nodes.service_node, scid, &htlc);
		match nodes.service_node.liquidity_manager.next_event() {
			Some(LiquidityEvent::LSPS4Service(LSPS4ServiceEvent::SendWebhook {
				counterparty_node_id,
				..
			})) => assert_eq!(counterparty_node_id, nodes.client_node.node.get_our_node_id()),
			e => panic!("Expected SendWebhook, got {:?}", e),
		}
	});
}

#[test]
fn connected_peer_honest_htlc_opens_channel() {
	run(|nodes, scid| {
		connect_client(nodes);
		let htlc = intercept_htlc(nodes, scid, 1, 0, AMOUNT_MSAT, None);
		handle(&nodes.service_node, scid, &htlc);
		match nodes.service_node.liquidity_manager.next_event() {
			Some(LiquidityEvent::LSPS4Service(LSPS4ServiceEvent::OpenChannel {
				their_network_key,
				amt_to_forward_msat,
				..
			})) => {
				assert_eq!(their_network_key, nodes.client_node.node.get_our_node_id());
				// 2% skim on the 10k sat HTLC.
				assert_eq!(amt_to_forward_msat, 9_800_000);
			},
			e => panic!("Expected OpenChannel, got {:?}", e),
		}
	});
}

#[test]
fn overpaid_htlc_is_accepted() {
	// Overpaying the LSP hop (e.g. a route hint with a non-zero fee) must keep working.
	run(|nodes, scid| {
		connect_client(nodes);
		let htlc = intercept_htlc(nodes, scid, 1, 101_000, AMOUNT_MSAT, None);
		handle(&nodes.service_node, scid, &htlc);
		assert!(matches!(
			nodes.service_node.liquidity_manager.next_event(),
			Some(LiquidityEvent::LSPS4Service(LSPS4ServiceEvent::OpenChannel { .. }))
		));
	});
}

/// Regression for the 2026-09-11 prod drain.
#[test]
fn lying_onion_never_opens_channel_or_forwards() {
	run(|nodes, scid| {
		connect_client(nodes);
		// 10 sat delivered, onion asks the LSP to forward 10k sat.
		let htlc = intercept_htlc(nodes, scid, 1, 0, 10_000, Some(AMOUNT_MSAT));
		// LDK itself does not catch it for intercept scids.
		assert_eq!(htlc.expected_outbound_amount_msat, AMOUNT_MSAT);

		handle(&nodes.service_node, scid, &htlc);
		assert!(nodes.service_node.liquidity_manager.next_event().is_none());
		assert_failed_back(&nodes.service_node, &htlc);

		// Not stored either: the retry timer and a reconnect must not pick it up later.
		let handler = nodes.service_node.liquidity_manager.lsps4_service_handler().unwrap();
		handler.process_pending_htlcs();
		connect_client(nodes);
		assert!(nodes.service_node.liquidity_manager.next_event().is_none());
	});
}

#[test]
fn lying_onion_to_offline_peer_is_rejected() {
	run(|nodes, scid| {
		let htlc = intercept_htlc(nodes, scid, 1, 0, AMOUNT_MSAT - 1, Some(AMOUNT_MSAT));
		handle(&nodes.service_node, scid, &htlc);
		assert!(nodes.service_node.liquidity_manager.next_event().is_none());
		assert_failed_back(&nodes.service_node, &htlc);
	});
}

#[test]
fn intercept_no_longer_pending_is_rejected() {
	// Unknown inbound amount must fail closed, not fall back to the onion's figure.
	run(|nodes, scid| {
		connect_client(nodes);
		let htlc = intercept_htlc(nodes, scid, 1, 0, AMOUNT_MSAT, None);
		nodes.service_node.node.fail_intercepted_htlc(htlc.id).unwrap();
		nodes.service_node.node.get_and_clear_pending_events();

		handle(&nodes.service_node, scid, &htlc);
		assert!(nodes.service_node.liquidity_manager.next_event().is_none());
	});
}
