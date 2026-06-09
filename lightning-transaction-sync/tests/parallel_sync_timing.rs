#![cfg(all(not(target_os = "windows"), feature = "esplora-async"))]

//! Self-contained before/after timing test for the parallelized Esplora tx_sync.
//!
//! Stands up a minimal mock Esplora HTTP server that injects a fixed latency on
//! the per-transaction `/merkleblock-proof` lookups (the fan-out the confirmed
//! sync parallelizes) and returns 404 for them, which esplora-client maps to
//! `Ok(None)` -> no confirmations, so the run is deterministic and needs no
//! bitcoind/electrs.
//!
//! With `N` watched transactions and `DELAY` per merkle lookup:
//!   - strictly-serial sync   ~= N * DELAY
//!   - parallel sync (buffer_unordered(C)) ~= ceil(N / C) * DELAY
//!
//! The test asserts the wall-time is well under the serial estimate, so it
//! PASSES on the parallel implementation and would FAIL on the old serial one.
//! That is the "before/after": run it on the base commit (serial) to see it
//! blow the bound, and on this branch (parallel) to see it pass.
//!
//! Correctness of actual confirmation handling / ordering is covered by the
//! electrs-backed `test_esplora_syncs` integration test (run in CI).

use lightning::chain::transaction::TransactionData;
use lightning::chain::{Confirm, Filter};
use lightning::util::test_utils::TestLogger;
use lightning_transaction_sync::EsploraSyncClient;

use bitcoin::block::Header;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::constants::genesis_block;
use bitcoin::hashes::Hash;
use bitcoin::network::Network;
use bitcoin::{BlockHash, ScriptBuf, Txid};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use std::time::{Duration, Instant};

// Number of watched transactions and the per-merkle-lookup latency injected by
// the mock. Chosen so serial (~N*DELAY = 4s) and parallel (~1s) are clearly
// separable with margin for CI jitter.
const N_TXS: usize = 40;
const DELAY_MS: u64 = 100;

// Minimal `Confirm` that reports nothing relevant, so `get_unconfirmed_transactions`
// has no work and the run is dominated by the confirmed-tx merkle fan-out.
struct NoopConfirmable;
impl Confirm for NoopConfirmable {
	fn transactions_confirmed(&self, _h: &Header, _txdata: &TransactionData, _height: u32) {}
	fn transaction_unconfirmed(&self, _txid: &Txid) {}
	fn best_block_updated(&self, _h: &Header, _height: u32) {}
	fn get_relevant_txids(&self) -> Vec<(Txid, u32, Option<BlockHash>)> {
		Vec::new()
	}
}

// Spawn a mock Esplora server on an ephemeral port; returns the port.
async fn spawn_mock_esplora() -> u16 {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let port = listener.local_addr().unwrap().port();

	// Precompute valid canned responses from the real genesis block.
	let genesis = genesis_block(Network::Bitcoin);
	let tip_hash_hex = genesis.block_hash().to_string(); // 64 hex chars
	let header_hex = serialize_hex(&genesis.header); // 80-byte header, 160 hex chars

	tokio::spawn(async move {
		loop {
			let (mut sock, _) = match listener.accept().await {
				Ok(x) => x,
				Err(_) => continue,
			};
			let tip = tip_hash_hex.clone();
			let hdr = header_hex.clone();
			tokio::spawn(async move {
				// Read until end of request headers.
				let mut buf = Vec::new();
				let mut tmp = [0u8; 1024];
				loop {
					match sock.read(&mut tmp).await {
						Ok(0) => break,
						Ok(n) => {
							buf.extend_from_slice(&tmp[..n]);
							if buf.windows(4).any(|w| w == b"\r\n\r\n") {
								break;
							}
						},
						Err(_) => return,
					}
				}
				let req = String::from_utf8_lossy(&buf);
				let path = req
					.lines()
					.next()
					.and_then(|l| l.split_whitespace().nth(1))
					.unwrap_or("/")
					.to_string();

				let (status, body): (&str, String) = if path == "/blocks/tip/hash" {
					("200 OK", tip.clone())
				} else if path.ends_with("/header") {
					("200 OK", hdr.clone())
				} else if path.ends_with("/status") {
					("200 OK", "{\"in_best_chain\":true,\"height\":100,\"next_best\":null}".into())
				} else if path.contains("/merkleblock-proof") {
					// The fan-out under test: inject latency, then 404 -> Ok(None).
					tokio::time::sleep(Duration::from_millis(DELAY_MS)).await;
					("404 Not Found", String::new())
				} else {
					("404 Not Found", String::new())
				};

				let resp = format!(
					"HTTP/1.1 {}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
					status,
					body.len(),
					body
				);
				let _ = sock.write_all(resp.as_bytes()).await;
				let _ = sock.shutdown().await;
			});
		}
	});

	port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_tx_sync_beats_serial_bound() {
	let port = spawn_mock_esplora().await;
	let url = format!("http://127.0.0.1:{}", port);
	let mut logger = TestLogger::new();
	let tx_sync = EsploraSyncClient::new(url, &mut logger);

	// Register N distinct watched transactions; each resolves to a 404 merkle
	// lookup (Ok(None)), so no confirmations are produced.
	let script = ScriptBuf::new();
	for i in 0..N_TXS {
		let txid = Txid::from_byte_array([(i as u8).wrapping_add(1); 32]);
		tx_sync.register_tx(&txid, script.as_script());
	}

	let confirmable = NoopConfirmable;
	let confirmables: Vec<&NoopConfirmable> = vec![&confirmable];

	let start = Instant::now();
	tx_sync.sync(confirmables).await.expect("sync should complete");
	let elapsed = start.elapsed();
	println!(
		"[parallel-timing] N={} delay={}ms -> sync took {}ms (serial estimate ~{}ms)",
		N_TXS,
		DELAY_MS,
		elapsed.as_millis(),
		(N_TXS as u64) * DELAY_MS
	);

	let serial_estimate_ms = (N_TXS as u64) * DELAY_MS;
	assert!(
		elapsed.as_millis() < (serial_estimate_ms / 2) as u128,
		"sync took {}ms; expected well under serial estimate {}ms -- parallel fan-out not happening?",
		elapsed.as_millis(),
		serial_estimate_ms
	);
}
