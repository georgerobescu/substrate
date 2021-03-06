// Copyright 2017-2018 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use protocol::Context;
use network_libp2p::{Severity, NodeIndex};
use client::{BlockStatus, ClientInfo};
use consensus::BlockOrigin;
use consensus::import_queue::{ImportQueue, IncomingBlock};
use client::error::Error as ClientError;
use blocks::BlockCollection;
use runtime_primitives::Justification;
use runtime_primitives::traits::{Block as BlockT, Header as HeaderT, As, NumberFor, Zero};
use runtime_primitives::generic::BlockId;
use message::{self, generic::Message as GenericMessage};
use config::Roles;

// Maximum blocks to request in a single packet.
const MAX_BLOCKS_TO_REQUEST: usize = 128;
// Maximum blocks to store in the import queue.
const MAX_IMPORTING_BLOCKS: usize = 2048;
// Number of blocks in the queue that prevents ancestry search.
const MAJOR_SYNC_BLOCKS: usize = 5;
// Time to wait before trying to get a justification from the same peer.
const JUSTIFICATION_RETRY_WAIT: Duration = Duration::from_secs(10);
// Number of recently announced blocks to track for each peer.
const ANNOUNCE_HISTORY_SIZE: usize = 64;
// Max number of blocks to download for unknown forks.
// TODO: this should take finality into account. See https://github.com/paritytech/substrate/issues/1606
const MAX_UNKNOWN_FORK_DOWNLOAD_LEN: u32 = 32;

struct PeerSync<B: BlockT> {
	pub common_number: NumberFor<B>,
	pub best_hash: B::Hash,
	pub best_number: NumberFor<B>,
	pub state: PeerSyncState<B>,
	pub recently_announced: VecDeque<B::Hash>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PeerSyncState<B: BlockT> {
	AncestorSearch(NumberFor<B>),
	Available,
	DownloadingNew(NumberFor<B>),
	DownloadingStale(B::Hash),
	DownloadingJustification(B::Hash),
}

/// Pending justification request for the given block (hash and number).
type PendingJustification<B> = (<B as BlockT>::Hash, NumberFor<B>);

/// Manages pending block justification requests.
struct PendingJustifications<B: BlockT> {
	justifications: HashSet<PendingJustification<B>>,
	pending_requests: VecDeque<PendingJustification<B>>,
	peer_requests: HashMap<NodeIndex, PendingJustification<B>>,
	previous_requests: HashMap<PendingJustification<B>, Vec<(NodeIndex, Instant)>>,
}

impl<B: BlockT> PendingJustifications<B> {
	fn new() -> PendingJustifications<B> {
		PendingJustifications {
			justifications: HashSet::new(),
			pending_requests: VecDeque::new(),
			peer_requests: HashMap::new(),
			previous_requests: HashMap::new(),
		}
	}

	/// Dispatches all possible pending requests to the given peers. Peers are
	/// filtered according to the current known best block (i.e. we won't send a
	/// justification request for block #10 to a peer at block #2), and we also
	/// throttle requests to the same peer if a previous justification request
	/// yielded no results.
	fn dispatch(&mut self, peers: &mut HashMap<NodeIndex, PeerSync<B>>, protocol: &mut Context<B>) {
		if self.pending_requests.is_empty() {
			return;
		}

		// clean up previous failed requests so we can retry again
		for (_, requests) in self.previous_requests.iter_mut() {
			requests.retain(|(_, instant)| instant.elapsed() < JUSTIFICATION_RETRY_WAIT);
		}

		let mut available_peers = peers.iter().filter_map(|(peer, sync)| {
			// don't request to any peers that already have pending requests or are unavailable
			if sync.state != PeerSyncState::Available || self.peer_requests.contains_key(&peer) {
				None
			} else {
				Some((*peer, sync.best_number))
			}
		}).collect::<VecDeque<_>>();

		let mut last_peer = available_peers.back().map(|p| p.0);
		let mut unhandled_requests = VecDeque::new();

		loop {
			let (peer, peer_best_number) = match available_peers.pop_front() {
				Some(p) => p,
				_ => break,
			};

			// only ask peers that have synced past the block number that we're
			// asking the justification for and to whom we haven't already made
			// the same request recently
			let peer_eligible = {
				let request = match self.pending_requests.front() {
					Some(r) => r.clone(),
					_ => break,
				};

				peer_best_number >= request.1 &&
					!self.previous_requests
						 .get(&request)
						 .map(|requests| requests.iter().any(|i| i.0 == peer))
						 .unwrap_or(false)
			};

			if !peer_eligible {
				available_peers.push_back((peer, peer_best_number));

				// we tried all peers and none can answer this request
				if Some(peer) == last_peer {
					last_peer = available_peers.back().map(|p| p.0);

					let request = self.pending_requests.pop_front()
						.expect("verified to be Some in the beginning of the loop; qed");

					unhandled_requests.push_back(request);
				}

				continue;
			}

			last_peer = available_peers.back().map(|p| p.0);

			let request = self.pending_requests.pop_front()
				.expect("verified to be Some in the beginning of the loop; qed");

			self.peer_requests.insert(peer, request);

			peers.get_mut(&peer)
				.expect("peer was is taken from available_peers; available_peers is a subset of peers; qed")
				.state = PeerSyncState::DownloadingJustification(request.0);

			trace!(target: "sync", "Requesting justification for block #{} from {}", request.0, peer);
			let request = message::generic::BlockRequest {
				id: 0,
				fields: message::BlockAttributes::JUSTIFICATION,
				from: message::FromBlock::Hash(request.0),
				to: None,
				direction: message::Direction::Ascending,
				max: Some(1),
			};

			protocol.send_message(peer, GenericMessage::BlockRequest(request));
		}

		self.pending_requests.append(&mut unhandled_requests);
	}

	/// Queue a justification request (without dispatching it).
	fn queue_request(&mut self, justification: &PendingJustification<B>) {
		if !self.justifications.insert(*justification) {
			return;
		}
		self.pending_requests.push_back(*justification);
	}

	/// Retry any pending request if a peer disconnected.
	fn peer_disconnected(&mut self, who: NodeIndex) {
		if let Some(request) = self.peer_requests.remove(&who) {
			self.pending_requests.push_front(request);
		}
	}

	/// Processes the response for the request previously sent to the given
	/// peer. Queues a retry in case the import fails or the given justification
	/// was `None`.
	fn on_response(
		&mut self,
		who: NodeIndex,
		justification: Option<Justification>,
		protocol: &mut Context<B>,
		import_queue: &ImportQueue<B>,
	) {
		// we assume that the request maps to the given response, this is
		// currently enforced by the outer network protocol before passing on
		// messages to chain sync.
		if let Some(request) = self.peer_requests.remove(&who) {
			if let Some(justification) = justification {
				if import_queue.import_justification(request.0, request.1, justification) {
					self.justifications.remove(&request);
					self.previous_requests.remove(&request);
					return;
				} else {
					protocol.report_peer(
						who,
						Severity::Bad(&format!("Invalid justification provided for #{}", request.0)),
					);
				}
			} else {
				self.previous_requests
					.entry(request)
					.or_insert(Vec::new())
					.push((who, Instant::now()));
			}

			self.pending_requests.push_front(request);
		}
	}

	/// Removes any pending justification requests for blocks lower than the
	/// given best finalized.
	fn collect_garbage(&mut self, best_finalized: NumberFor<B>) {
		self.justifications.retain(|(_, n)| *n > best_finalized);
		self.pending_requests.retain(|(_, n)| *n > best_finalized);
		self.peer_requests.retain(|_, (_, n)| *n > best_finalized);
		self.previous_requests.retain(|(_, n), _| *n > best_finalized);
	}
}

/// Relay chain sync strategy.
pub struct ChainSync<B: BlockT> {
	genesis_hash: B::Hash,
	peers: HashMap<NodeIndex, PeerSync<B>>,
	blocks: BlockCollection<B>,
	best_queued_number: NumberFor<B>,
	best_queued_hash: B::Hash,
	required_block_attributes: message::BlockAttributes,
	import_queue: Arc<ImportQueue<B>>,
	justifications: PendingJustifications<B>,
}

/// Reported sync state.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum SyncState {
	/// Initial sync is complete, keep-up sync is active.
	Idle,
	/// Actively catching up with the chain.
	Downloading
}

/// Syncing status and statistics
#[derive(Clone)]
pub struct Status<B: BlockT> {
	/// Current global sync state.
	pub state: SyncState,
	/// Target sync block number.
	pub best_seen_block: Option<NumberFor<B>>,
}

impl<B: BlockT> Status<B> {
	/// Whether the synchronization status is doing major downloading work or
	/// is near the head of the chain.
	pub fn is_major_syncing(&self) -> bool {
		match self.state {
			SyncState::Idle => false,
			SyncState::Downloading => true,
		}
	}
}

impl<B: BlockT> ChainSync<B> {
	/// Create a new instance.
	pub(crate) fn new(role: Roles, info: &ClientInfo<B>, import_queue: Arc<ImportQueue<B>>) -> Self {
		let mut required_block_attributes = message::BlockAttributes::HEADER | message::BlockAttributes::JUSTIFICATION;
		if role.intersects(Roles::FULL | Roles::AUTHORITY) {
			required_block_attributes |= message::BlockAttributes::BODY;
		}

		ChainSync {
			genesis_hash: info.chain.genesis_hash,
			peers: HashMap::new(),
			blocks: BlockCollection::new(),
			best_queued_hash: info.best_queued_hash.unwrap_or(info.chain.best_hash),
			best_queued_number: info.best_queued_number.unwrap_or(info.chain.best_number),
			justifications: PendingJustifications::new(),
			required_block_attributes,
			import_queue,
		}
	}

	fn best_seen_block(&self) -> Option<NumberFor<B>> {
		self.peers.values().max_by_key(|p| p.best_number).map(|p| p.best_number)
	}

	/// Returns import queue reference.
	pub(crate) fn import_queue(&self) -> Arc<ImportQueue<B>> {
		self.import_queue.clone()
	}

	/// Returns sync status.
	pub(crate) fn status(&self) -> Status<B> {
		let best_seen = self.best_seen_block();
		let state = match &best_seen {
			&Some(n) if n > self.best_queued_number && n - self.best_queued_number > As::sa(5) => SyncState::Downloading,
			_ => SyncState::Idle,
		};
		Status {
			state: state,
			best_seen_block: best_seen,
		}
	}

	/// Handle new connected peer.
	pub(crate) fn new_peer(&mut self, protocol: &mut Context<B>, who: NodeIndex) {
		if let Some(info) = protocol.peer_info(who) {
			match (block_status(&*protocol.client(), &*self.import_queue, info.best_hash), info.best_number) {
				(Err(e), _) => {
					debug!(target:"sync", "Error reading blockchain: {:?}", e);
					protocol.report_peer(who, Severity::Useless(&format!("Error legimimately reading blockchain status: {:?}", e)));
				},
				(Ok(BlockStatus::KnownBad), _) => {
					protocol.report_peer(who, Severity::Bad(&format!("New peer with known bad best block {} ({}).", info.best_hash, info.best_number)));
				},
				(Ok(BlockStatus::Unknown), b) if b.is_zero() => {
					protocol.report_peer(who, Severity::Bad(&format!("New peer with unknown genesis hash {} ({}).", info.best_hash, info.best_number)));
				},
				(Ok(BlockStatus::Unknown), _) if self.import_queue.status().importing_count > MAJOR_SYNC_BLOCKS => {
					// when actively syncing the common point moves too fast.
					debug!(target:"sync", "New peer with unknown best hash {} ({}), assuming common block.", self.best_queued_hash, self.best_queued_number);
					self.peers.insert(who, PeerSync {
						common_number: self.best_queued_number,
						best_hash: info.best_hash,
						best_number: info.best_number,
						state: PeerSyncState::Available,
						recently_announced: Default::default(),
					});
				}
				(Ok(BlockStatus::Unknown), _) => {
					let our_best = self.best_queued_number;
					if our_best > As::sa(0) {
						let common_best = ::std::cmp::min(our_best, info.best_number);
						debug!(target:"sync", "New peer with unknown best hash {} ({}), searching for common ancestor.", info.best_hash, info.best_number);
						self.peers.insert(who, PeerSync {
							common_number: As::sa(0),
							best_hash: info.best_hash,
							best_number: info.best_number,
							state: PeerSyncState::AncestorSearch(common_best),
							recently_announced: Default::default(),
						});
						Self::request_ancestry(protocol, who, common_best)
					} else {
						// We are at genesis, just start downloading
						debug!(target:"sync", "New peer with best hash {} ({}).", info.best_hash, info.best_number);
						self.peers.insert(who, PeerSync {
							common_number: As::sa(0),
							best_hash: info.best_hash,
							best_number: info.best_number,
							state: PeerSyncState::Available,
							recently_announced: Default::default(),
						});
						self.download_new(protocol, who)
					}
				},
				(Ok(BlockStatus::Queued), _) | (Ok(BlockStatus::InChain), _) => {
					debug!(target:"sync", "New peer with known best hash {} ({}).", info.best_hash, info.best_number);
					self.peers.insert(who, PeerSync {
						common_number: info.best_number,
						best_hash: info.best_hash,
						best_number: info.best_number,
						state: PeerSyncState::Available,
						recently_announced: Default::default(),
					});
				}
			}
		}
	}

	/// Handle new block data.
	pub(crate) fn on_block_data(
		&mut self,
		protocol: &mut Context<B>,
		who: NodeIndex,
		request: message::BlockRequest<B>,
		response: message::BlockResponse<B>
	) -> Option<(BlockOrigin, Vec<IncomingBlock<B>>)> {
		let new_blocks: Vec<IncomingBlock<B>> = if let Some(ref mut peer) = self.peers.get_mut(&who) {
			let mut blocks = response.blocks;
			if request.direction == message::Direction::Descending {
				trace!(target: "sync", "Reversing incoming block list");
				blocks.reverse();
			}
			match peer.state {
				PeerSyncState::DownloadingNew(start_block) => {
					self.blocks.clear_peer_download(who);
					peer.state = PeerSyncState::Available;
					self.blocks.insert(start_block, blocks, who);
					self.blocks
						.drain(self.best_queued_number + As::sa(1))
						.into_iter()
						.map(|block_data| {
							IncomingBlock {
								hash: block_data.block.hash,
								header: block_data.block.header,
								body: block_data.block.body,
								justification: block_data.block.justification,
								origin: block_data.origin,
							}
						}).collect()
				},
				PeerSyncState::DownloadingStale(_) => {
					peer.state = PeerSyncState::Available;
					blocks.into_iter().map(|b| {
						IncomingBlock {
							hash: b.hash,
							header: b.header,
							body: b.body,
							justification: b.justification,
							origin: Some(who),
						}
					}).collect()
				},
				PeerSyncState::AncestorSearch(n) => {
					match blocks.get(0) {
						Some(ref block) => {
							trace!(target: "sync", "Got ancestry block #{} ({}) from peer {}", n, block.hash, who);
							match protocol.client().block_hash(n) {
								Ok(Some(block_hash)) if block_hash == block.hash => {
									if peer.common_number < n {
										peer.common_number = n;
									}
									peer.state = PeerSyncState::Available;
									trace!(target:"sync", "Found common ancestor for peer {}: {} ({})", who, block.hash, n);
									vec![]
								},
								Ok(our_best) if n > As::sa(0) => {
									trace!(target:"sync", "Ancestry block mismatch for peer {}: theirs: {} ({}), ours: {:?}", who, block.hash, n, our_best);
									let n = n - As::sa(1);
									peer.state = PeerSyncState::AncestorSearch(n);
									Self::request_ancestry(protocol, who, n);
									return None;
								},
								Ok(_) => { // genesis mismatch
									trace!(target:"sync", "Ancestry search: genesis mismatch for peer {}", who);
									protocol.report_peer(who, Severity::Bad("Ancestry search: genesis mismatch for peer"));
									return None;
								},
								Err(e) => {
									protocol.report_peer(who, Severity::Useless(&format!("Error answering legitimate blockchain query: {:?}", e)));
									return None;
								}
							}
						},
						None => {
							trace!(target:"sync", "Invalid response when searching for ancestor from {}", who);
							protocol.report_peer(who, Severity::Bad("Invalid response when searching for ancestor"));
							return None;
						}
					}
				},
				PeerSyncState::Available | PeerSyncState::DownloadingJustification(..) => Vec::new(),
			}
		} else {
			Vec::new()
		};

		let is_recent = new_blocks
			.first()
			.map(|block| self.peers.iter().any(|(_, peer)| peer.recently_announced.contains(&block.hash)))
			.unwrap_or(false);
		let origin = if is_recent { BlockOrigin::NetworkBroadcast } else { BlockOrigin::NetworkInitialSync };

		if let Some((hash, number)) = new_blocks.last()
			.and_then(|b| b.header.as_ref().map(|h| (b.hash.clone(), *h.number())))
		{
			trace!(target:"sync", "Accepted {} blocks ({:?}) with origin {:?}", new_blocks.len(), hash, origin);
			self.block_queued(&hash, number);
		}
		self.maintain_sync(protocol);
		Some((origin, new_blocks))
	}

	/// Handle new justification data.
	pub(crate) fn on_block_justification_data(
		&mut self,
		protocol: &mut Context<B>,
		who: NodeIndex,
		_request: message::BlockRequest<B>,
		response: message::BlockResponse<B>,
	) {
		if let Some(ref mut peer) = self.peers.get_mut(&who) {
			if let PeerSyncState::DownloadingJustification(hash) = peer.state {
				peer.state = PeerSyncState::Available;

				// we only request one justification at a time
				match response.blocks.into_iter().next() {
					Some(response) => {
						if hash != response.hash {
							let msg = format!(
								"Invalid block justification provided: requested: {:?} got: {:?}",
								hash,
								response.hash,
							);

							protocol.report_peer(who, Severity::Bad(&msg));
							return;
						}

						self.justifications.on_response(
							who,
							response.justification,
							protocol,
							&*self.import_queue,
						);
					},
					None => {
						let msg = format!(
							"Provided empty response for justification request {:?}",
							hash,
						);

						protocol.report_peer(who, Severity::Useless(&msg));
						return;
					},
				}
			}
		}

		self.maintain_sync(protocol);
	}

	/// Maintain the sync process (download new blocks, fetch justifications).
	pub fn maintain_sync(&mut self, protocol: &mut Context<B>) {
		let peers: Vec<NodeIndex> = self.peers.keys().map(|p| *p).collect();
		for peer in peers {
			self.download_new(protocol, peer);
		}
		self.justifications.dispatch(&mut self.peers, protocol);
	}

	/// Called periodically to perform any time-based actions.
	pub fn tick(&mut self, protocol: &mut Context<B>) {
		self.justifications.dispatch(&mut self.peers, protocol);
	}

	/// Request a justification for the given block.
	///
	/// Queues a new justification request and tries to dispatch all pending requests.
	pub fn request_justification(&mut self, hash: &B::Hash, number: NumberFor<B>, protocol: &mut Context<B>) {
		self.justifications.queue_request(&(*hash, number));
		self.justifications.dispatch(&mut self.peers, protocol);
	}

	/// Notify about successful import of the given block.
	pub fn block_imported(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		trace!(target: "sync", "Block imported successfully {} ({})", number, hash);
	}

	/// Notify about finalization of the given block.
	pub fn block_finalized(&mut self, _hash: &B::Hash, number: NumberFor<B>) {
		self.justifications.collect_garbage(number);
	}

	fn block_queued(&mut self, hash: &B::Hash, number: NumberFor<B>) {
		if number > self.best_queued_number {
			self.best_queued_number = number;
			self.best_queued_hash = *hash;
		}
		// Update common blocks
		for (n, peer) in self.peers.iter_mut() {
			if let PeerSyncState::AncestorSearch(_) = peer.state {
				// Abort search.
				peer.state = PeerSyncState::Available;
			}
			trace!(target: "sync", "Updating peer {} info, ours={}, common={}, their best={}", n, number, peer.common_number, peer.best_number);
			if peer.best_number >= number {
				peer.common_number = number;
			} else {
				peer.common_number = peer.best_number;
			}
		}
	}

	pub(crate) fn update_chain_info(&mut self, best_header: &B::Header) {
		let hash = best_header.hash();
		self.block_queued(&hash, best_header.number().clone())
	}

	/// Handle new block announcement.
	pub(crate) fn on_block_announce(&mut self, protocol: &mut Context<B>, who: NodeIndex, hash: B::Hash, header: &B::Header) {
		let number = *header.number();
		if number <= As::sa(0) {
			trace!(target: "sync", "Ignored invalid block announcement from {}: {}", who, hash);
			return;
		}
		let known_parent = self.is_known(protocol, &header.parent_hash());
		let known = self.is_known(protocol, &hash);
		if let Some(ref mut peer) = self.peers.get_mut(&who) {
			while peer.recently_announced.len() >= ANNOUNCE_HISTORY_SIZE {
				peer.recently_announced.pop_front();
			}
			peer.recently_announced.push_back(hash.clone());
			if number > peer.best_number {
				// update their best block
				peer.best_number = number;
				peer.best_hash = hash;
			}
			if let PeerSyncState::AncestorSearch(_) = peer.state {
				return;
			}
			if header.parent_hash() == &self.best_queued_hash || known_parent {
				peer.common_number = number - As::sa(1);
			} else if known {
				peer.common_number = number
			}
		} else {
			return;
		}

		if !(known || self.is_already_downloading(&hash)) {
			let stale = number <= self.best_queued_number;
			if stale {
				if !(known_parent || self.is_already_downloading(header.parent_hash())) {
					trace!(target: "sync", "Considering new unknown stale block announced from {}: {} {:?}", who, hash, header);
					self.download_unknown_stale(protocol, who, &hash);
				} else {
					trace!(target: "sync", "Considering new stale block announced from {}: {} {:?}", who, hash, header);
					self.download_stale(protocol, who, &hash);
				}
			} else {
				trace!(target: "sync", "Considering new block announced from {}: {} {:?}", who, hash, header);
				self.download_new(protocol, who);
			}
		} else {
			trace!(target: "sync", "Known block announce from {}: {}", who, hash);
		}
	}

	fn is_already_downloading(&self, hash: &B::Hash) -> bool {
		self.peers.iter().any(|(_, p)| p.state == PeerSyncState::DownloadingStale(*hash))
	}

	fn is_known(&self, protocol: &mut Context<B>, hash: &B::Hash) -> bool {
		block_status(&*protocol.client(), &*self.import_queue, *hash).ok().map_or(false, |s| s != BlockStatus::Unknown)
	}

	/// Handle disconnected peer.
	pub(crate) fn peer_disconnected(&mut self, protocol: &mut Context<B>, who: NodeIndex) {
		self.blocks.clear_peer_download(who);
		self.peers.remove(&who);
		self.justifications.peer_disconnected(who);
		self.maintain_sync(protocol);
	}

	/// Restart the sync process.
	pub(crate) fn restart(&mut self, protocol: &mut Context<B>) {
		self.import_queue.clear();
		self.blocks.clear();
		match protocol.client().info() {
			Ok(info) => {
				self.best_queued_hash = info.best_queued_hash.unwrap_or(info.chain.best_hash);
				self.best_queued_number = info.best_queued_number.unwrap_or(info.chain.best_number);
				debug!(target:"sync", "Restarted with {} ({})", self.best_queued_number, self.best_queued_hash);
			},
			Err(e) => {
				debug!(target:"sync", "Error reading blockchain: {:?}", e);
				self.best_queued_hash = self.genesis_hash;
				self.best_queued_number = As::sa(0);
			}
		}
		let ids: Vec<NodeIndex> = self.peers.drain().map(|(id, _)| id).collect();
		for id in ids {
			self.new_peer(protocol, id);
		}
	}

	/// Clear all sync data.
	pub(crate) fn clear(&mut self) {
		self.blocks.clear();
		self.peers.clear();
	}

	// Download old block with known parent.
	fn download_stale(&mut self, protocol: &mut Context<B>, who: NodeIndex, hash: &B::Hash) {
		if let Some(ref mut peer) = self.peers.get_mut(&who) {
			match peer.state {
				PeerSyncState::Available => {
					let request = message::generic::BlockRequest {
						id: 0,
						fields: self.required_block_attributes.clone(),
						from: message::FromBlock::Hash(*hash),
						to: None,
						direction: message::Direction::Ascending,
						max: Some(1),
					};
					peer.state = PeerSyncState::DownloadingStale(*hash);
					protocol.send_message(who, GenericMessage::BlockRequest(request));
				},
				_ => (),
			}
		}
	}

	// Download old block with unknown parent.
	fn download_unknown_stale(&mut self, protocol: &mut Context<B>, who: NodeIndex, hash: &B::Hash) {
		if let Some(ref mut peer) = self.peers.get_mut(&who) {
			match peer.state {
				PeerSyncState::Available => {
					let request = message::generic::BlockRequest {
						id: 0,
						fields: self.required_block_attributes.clone(),
						from: message::FromBlock::Hash(*hash),
						to: None,
						direction: message::Direction::Descending,
						max: Some(MAX_UNKNOWN_FORK_DOWNLOAD_LEN),
					};
					peer.state = PeerSyncState::DownloadingStale(*hash);
					protocol.send_message(who, GenericMessage::BlockRequest(request));
				},
				_ => (),
			}
		}
	}

	// Issue a request for a peer to download new blocks, if any are available
	fn download_new(&mut self, protocol: &mut Context<B>, who: NodeIndex) {
		if let Some(ref mut peer) = self.peers.get_mut(&who) {
			let import_status = self.import_queue.status();
			// when there are too many blocks in the queue => do not try to download new blocks
			if import_status.importing_count > MAX_IMPORTING_BLOCKS {
				trace!(target: "sync", "Too many blocks in the queue.");
				return;
			}
			match peer.state {
				PeerSyncState::Available => {
					trace!(target: "sync", "Considering new block download from {}, common block is {}, best is {:?}", who, peer.common_number, peer.best_number);
					if let Some(range) = self.blocks.needed_blocks(who, MAX_BLOCKS_TO_REQUEST, peer.best_number, peer.common_number) {
						trace!(target: "sync", "Requesting blocks from {}, ({} to {})", who, range.start, range.end);
						let request = message::generic::BlockRequest {
							id: 0,
							fields: self.required_block_attributes.clone(),
							from: message::FromBlock::Number(range.start),
							to: None,
							direction: message::Direction::Ascending,
							max: Some((range.end - range.start).as_() as u32),
						};
						peer.state = PeerSyncState::DownloadingNew(range.start);
						protocol.send_message(who, GenericMessage::BlockRequest(request));
					} else {
						trace!(target: "sync", "Nothing to request");
					}
				},
				_ => trace!(target: "sync", "Peer {} is busy", who),
			}
		}
	}

	fn request_ancestry(protocol: &mut Context<B>, who: NodeIndex, block: NumberFor<B>) {
		trace!(target: "sync", "Requesting ancestry block #{} from {}", block, who);
		let request = message::generic::BlockRequest {
			id: 0,
			fields: message::BlockAttributes::HEADER | message::BlockAttributes::JUSTIFICATION,
			from: message::FromBlock::Number(block),
			to: None,
			direction: message::Direction::Ascending,
			max: Some(1),
		};
		protocol.send_message(who, GenericMessage::BlockRequest(request));
	}
}

/// Get block status, taking into account import queue.
fn block_status<B: BlockT>(
	chain: &::chain::Client<B>,
	queue: &ImportQueue<B>,
	hash: B::Hash) -> Result<BlockStatus, ClientError>
{
	if queue.is_importing(&hash) {
		return Ok(BlockStatus::Queued);
	}

	chain.block_status(&BlockId::Hash(hash))
}
