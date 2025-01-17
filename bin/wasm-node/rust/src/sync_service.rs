// Smoldot
// Copyright (C) 2019-2021  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Background syncing service.
//!
//! The role of the [`SyncService`] is to do whatever necessary to obtain and stay up-to-date
//! with the best and the finalized blocks of a chain.
//!
//! The configuration of the chain to synchronize must be passed when creating a [`SyncService`],
//! after which it will spawn background tasks and use the networking service to stay
//! synchronized.
//!
//! Use [`SyncService::subscribe_best`] and [`SyncService::subscribe_finalized`] to get notified
//! about updates of the best and finalized blocks.

use crate::{ffi, lossy_channel, network_service, runtime_service};

use blake2_rfc::blake2b::blake2b;
use futures::{
    channel::{mpsc, oneshot},
    lock::Mutex,
    prelude::*,
};
use smoldot::{
    chain, header,
    informant::HashDisplay,
    libp2p, network,
    sync::{all, para},
    trie::proof_verify,
};
use std::{collections::HashMap, convert::TryFrom as _, num::NonZeroU32, pin::Pin, sync::Arc};

pub use crate::lossy_channel::Receiver as NotificationsReceiver;

/// Configuration for a [`SyncService`].
pub struct Config {
    /// State of the finalized chain.
    pub chain_information: chain::chain_information::ChainInformation,

    /// Closure that spawns background tasks.
    pub tasks_executor: Box<dyn FnMut(Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,

    /// Access to the network, and index of the chain to sync from the point of view of the
    /// network service.
    pub network_service: (Arc<network_service::NetworkService>, usize),

    /// Receiver for events coming from the network, as returned by
    /// [`network_service::NetworkService::new`].
    pub network_events_receiver: mpsc::Receiver<network_service::Event>,

    /// Extra fields used when the chain is a parachain.
    /// If `None`, this chain is a standalone chain or a relay chain.
    pub parachain: Option<ConfigParachain>,
}

/// See [`Config::parachain`].
pub struct ConfigParachain {
    /// Runtime service that synchronizes the relay chain of this parachain.
    pub relay_chain_sync: Arc<runtime_service::RuntimeService>,

    /// Index in the network service of [`Config::network_service`] of the relay chain of this
    /// parachain.
    pub relay_network_chain_index: usize,

    /// Id of the parachain within the relay chain.
    ///
    /// This is an arbitrary number used to identify the parachain within the storage of the
    /// relay chain.
    ///
    /// > **Note**: This information is normally found in the chain specification of the
    /// >           parachain.
    pub parachain_id: u32,
}

/// Identifier for a blocks request to be performed.
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct BlocksRequestId(usize);

pub struct SyncService {
    /// Sender of messages towards the background task.
    to_background: Mutex<mpsc::Sender<ToBackground>>,
}

impl SyncService {
    pub async fn new(mut config: Config) -> Self {
        let (to_background, from_foreground) = mpsc::channel(16);

        if let Some(config_parachain) = config.parachain {
            (config.tasks_executor)(Box::pin(start_parachain(
                config.chain_information,
                from_foreground,
                config_parachain,
            )));
        } else {
            (config.tasks_executor)(Box::pin(
                start_relay_chain(
                    config.chain_information,
                    from_foreground,
                    config.network_service.0,
                    config.network_service.1,
                    config.network_events_receiver,
                )
                .await,
            ));
        }

        SyncService {
            to_background: Mutex::new(to_background),
        }
    }

    /// Returns the SCALE-encoded header of the current finalized block, alongside with a stream
    /// producing updates of the finalized block.
    ///
    /// Not all updates are necessarily reported. In particular, updates that weren't pulled from
    /// the `Stream` yet might get overwritten by newest updates.
    pub async fn subscribe_finalized(&self) -> (Vec<u8>, NotificationsReceiver<Vec<u8>>) {
        let (send_back, rx) = oneshot::channel();

        self.to_background
            .lock()
            .await
            .send(ToBackground::SubscribeFinalized { send_back })
            .await
            .unwrap();

        rx.await.unwrap()
    }

    /// Returns the SCALE-encoded header of the current best block, alongside with a stream
    /// producing updates of the best block.
    ///
    /// Not all updates are necessarily reported. In particular, updates that weren't pulled from
    /// the `Stream` yet might get overwritten by newest updates.
    pub async fn subscribe_best(&self) -> (Vec<u8>, NotificationsReceiver<Vec<u8>>) {
        let (send_back, rx) = oneshot::channel();

        self.to_background
            .lock()
            .await
            .send(ToBackground::SubscribeBest { send_back })
            .await
            .unwrap();

        rx.await.unwrap()
    }

    /// Returns true if it is believed that we are near the head of the chain.
    ///
    /// The way this method is implemented is opaque and cannot be relied on. The return value
    /// should only ever be shown to the user and not used for any meaningful logic.
    pub async fn is_near_head_of_chain_heuristic(&self) -> bool {
        let (send_back, rx) = oneshot::channel();

        self.to_background
            .lock()
            .await
            .send(ToBackground::IsNearHeadOfChainHeuristic { send_back })
            .await
            .unwrap();

        rx.await.unwrap()
    }
}

async fn start_relay_chain(
    chain_information: chain::chain_information::ChainInformation,
    mut from_foreground: mpsc::Receiver<ToBackground>,
    network_service: Arc<network_service::NetworkService>,
    network_chain_index: usize,
    mut from_network_service: mpsc::Receiver<network_service::Event>,
) -> impl Future<Output = ()> {
    // TODO: implicit generics
    let mut sync = all::AllSync::<(), libp2p::PeerId, ()>::new(all::Config {
        chain_information,
        sources_capacity: 32,
        source_selection_randomness_seed: rand::random(),
        blocks_request_granularity: NonZeroU32::new(128).unwrap(),
        blocks_capacity: {
            // This is the maximum number of blocks between two consecutive justifications.
            1024
        },
        download_ahead_blocks: {
            // Verifying a block mostly consists in:
            //
            // - Verifying a sr25519 signature for each block, plus a VRF output when the
            // block is claiming a primary BABE slot.
            // - Verifying one ed25519 signature per authority for every justification.
            //
            // At the time of writing, the speed of these operations hasn't been benchmarked.
            // It is likely that it varies quite a bit between the various environments (the
            // different browser engines, and NodeJS).
            //
            // Assuming a maximum verification speed of 5k blocks/sec and a 95% latency of one
            // second, the number of blocks to download ahead of time in order to not block
            // is 5k.
            5000
        },
        full: None,
    });

    async move {
        // TODO: remove
        let mut peers_source_id_map = HashMap::new();

        // List of block requests currently in progress.
        let mut pending_block_requests = stream::FuturesUnordered::new();
        // List of grandpa warp sync requests currently in progress.
        let mut pending_grandpa_requests = stream::FuturesUnordered::new();
        // List of storage requests currently in progress.
        let mut pending_storage_requests = stream::FuturesUnordered::new();

        // TODO: remove; should store the aborthandle in the TRq user data instead
        let mut pending_requests = HashMap::new();

        // TODO: finalized isn't notified
        let mut finalized_notifications = Vec::<lossy_channel::Sender<Vec<u8>>>::new();
        let mut best_notifications = Vec::<lossy_channel::Sender<Vec<u8>>>::new();

        // Queue of requests that the sync state machine wants to start and that haven't been
        // sent out yet.
        let mut requests_to_start = Vec::<all::Action>::with_capacity(16);

        // TODO: crappy way to handle that
        let mut has_new_best = false;
        let mut has_new_finalized = false;

        // Main loop of the syncing logic.
        loop {
            // Drain the content of `requests_to_start` to actually start the requests that have
            // been queued by the previous iteration of the main loop.
            //
            // Note that the code below assumes that the `source_id`s found in `requests_to_start`
            // still exist. This is only guaranteed to be case because we process
            // `requests_to_start` as soon as an entry is added and before disconnect events can
            // remove sources from the state machine.
            if let all::AllSync::Idle(ref mut sync_idle) = &mut sync {
                for request in requests_to_start.drain(..) {
                    match request {
                        all::Action::Start {
                            source_id,
                            request_id,
                            detail:
                                all::RequestDetail::BlocksRequest {
                                    first_block,
                                    ascending,
                                    num_blocks,
                                    request_headers,
                                    request_bodies,
                                    request_justification,
                                },
                        } => {
                            let peer_id = sync_idle.source_user_data_mut(source_id).clone();

                            let block_request = network_service.clone().blocks_request(
                                peer_id.clone(),
                                network_chain_index,
                                network::protocol::BlocksRequestConfig {
                                    start: match first_block {
                                        all::BlocksRequestFirstBlock::Hash(h) => {
                                            network::protocol::BlocksRequestConfigStart::Hash(h)
                                        }
                                        all::BlocksRequestFirstBlock::Number(n) => {
                                            network::protocol::BlocksRequestConfigStart::Number(n)
                                        }
                                    },
                                    desired_count: NonZeroU32::new(
                                        u32::try_from(num_blocks.get()).unwrap_or(u32::max_value()),
                                    )
                                    .unwrap(),
                                    direction: if ascending {
                                        network::protocol::BlocksRequestDirection::Ascending
                                    } else {
                                        network::protocol::BlocksRequestDirection::Descending
                                    },
                                    fields: network::protocol::BlocksRequestFields {
                                        header: request_headers,
                                        body: request_bodies,
                                        justification: request_justification,
                                    },
                                },
                            );

                            let (block_request, abort) = future::abortable(block_request);
                            pending_requests.insert(request_id, abort);

                            pending_block_requests
                                .push(async move { (request_id, block_request.await) });
                        }
                        all::Action::Start {
                            source_id,
                            request_id,
                            detail:
                                all::RequestDetail::GrandpaWarpSync {
                                    sync_start_block_hash,
                                },
                        } => {
                            let peer_id = sync_idle.source_user_data_mut(source_id).clone();

                            let grandpa_request =
                                network_service.clone().grandpa_warp_sync_request(
                                    peer_id.clone(),
                                    network_chain_index,
                                    sync_start_block_hash,
                                );

                            let (grandpa_request, abort) = future::abortable(grandpa_request);
                            pending_requests.insert(request_id, abort);

                            pending_grandpa_requests
                                .push(async move { (request_id, grandpa_request.await) });
                        }
                        all::Action::Start {
                            source_id,
                            request_id,
                            detail:
                                all::RequestDetail::StorageGet {
                                    block_hash,
                                    state_trie_root,
                                    keys,
                                },
                        } => {
                            let peer_id = sync_idle.source_user_data_mut(source_id).clone();

                            let storage_request = network_service.clone().storage_proof_request(
                                network_chain_index,
                                peer_id.clone(),
                                network::protocol::StorageProofRequestConfig {
                                    block_hash,
                                    keys: keys.clone().into_iter(),
                                },
                            );

                            let storage_request = async move {
                                if let Ok(outcome) = storage_request.await {
                                    // TODO: lots of copying around
                                    // TODO: log what happens
                                    keys.into_iter()
                                        .map(|key| {
                                            proof_verify::verify_proof(
                                                proof_verify::VerifyProofConfig {
                                                    proof: outcome.iter().map(|nv| &nv[..]),
                                                    requested_key: key.as_ref(),
                                                    trie_root_hash: &state_trie_root,
                                                },
                                            )
                                            .map_err(|_err| {
                                                panic!("{:?}", _err); // TODO: remove panic, it's just for debugging
                                                ()
                                            })
                                            .map(|v| v.map(|v| v.to_vec()))
                                        })
                                        .collect::<Result<Vec<_>, ()>>()
                                } else {
                                    Err(())
                                }
                            };

                            let (storage_request, abort) = future::abortable(storage_request);
                            pending_requests.insert(request_id, abort);

                            pending_storage_requests
                                .push(async move { (request_id, storage_request.await) });
                        }
                        all::Action::Cancel(request_id) => {
                            pending_requests.remove(&request_id).unwrap().abort();
                        }
                    }
                }
            }

            // The sync state machine can be in a few various states. At the time of writing:
            // idle, verifying header, verifying block, verifying grandpa warp sync proof,
            // verifying storage proof.
            // If the state is one of the "verifying" states, perform the actual verification and
            // loop again until the sync is in an idle state.
            let mut sync_idle: all::Idle<_, _, _> = match sync {
                all::AllSync::Idle(idle) => idle,
                all::AllSync::HeaderVerify(verify) => match verify.perform(ffi::unix_time(), ()) {
                    all::HeaderVerifyOutcome::Success {
                        sync: sync_idle,
                        next_actions,
                        is_new_best,
                        is_new_finalized,
                        ..
                    } => {
                        requests_to_start.extend(next_actions);

                        if is_new_best {
                            has_new_best = true;
                        }
                        if is_new_finalized {
                            has_new_finalized = true;
                        }

                        sync = sync_idle.into();
                        continue;
                    }
                    all::HeaderVerifyOutcome::Error {
                        sync: sync_idle,
                        next_actions,
                        error,
                        ..
                    } => {
                        log::warn!(
                            target: "sync-verify",
                            "Error while verifying header: {}",
                            error
                        );
                        requests_to_start.extend(next_actions);
                        sync = sync_idle.into();
                        continue;
                    }
                },
            };

            // TODO: handle this differently
            if has_new_best {
                has_new_best = false;

                let scale_encoded_header = sync_idle.best_block_header().scale_encoding_vec();
                // TODO: remove expired senders
                for notif in &mut best_notifications {
                    let _ = notif.send(scale_encoded_header.clone());
                }

                // Since this task is verifying blocks, a heavy CPU-only operation, it is very
                // much possible for it to take a long time before having to wait for some event.
                // Since JavaScript/Wasm is single-threaded, this would prevent all the other
                // tasks in the background from running.
                // In order to provide a better granularity, we force a yield after each new serie
                // of verifications.
                crate::yield_once().await;
            }

            // TODO: handle this differently
            if has_new_finalized {
                has_new_finalized = false;

                // If the chain uses GrandPa, the networking has to be kept up-to-date with the
                // state of finalization for other peers to send back relevant gossip messages.
                // (code style) `grandpa_set_id` is extracted first in order to avoid borrowing
                // checker issues.
                let grandpa_set_id =
                    if let chain::chain_information::ChainInformationFinalityRef::Grandpa {
                        after_finalized_block_authorities_set_id,
                        ..
                    } = sync_idle.as_chain_information().finality
                    {
                        Some(after_finalized_block_authorities_set_id)
                    } else {
                        None
                    };
                if let Some(set_id) = grandpa_set_id {
                    let commit_finalized_height =
                        u32::try_from(sync_idle.finalized_block_header().number).unwrap(); // TODO: unwrap :-/
                    network_service
                        .set_local_grandpa_state(
                            network_chain_index,
                            network::service::GrandpaState {
                                set_id,
                                round_number: 1, // TODO:
                                commit_finalized_height,
                            },
                        )
                        .await;
                }

                let scale_encoded_header = sync_idle.finalized_block_header().scale_encoding_vec();
                // TODO: remove expired senders
                for notif in &mut finalized_notifications {
                    let _ = notif.send(scale_encoded_header.clone());
                }

                // Since this task is verifying blocks, a heavy CPU-only operation, it is very
                // much possible for it to take a long time before having to wait for some event.
                // Since JavaScript/Wasm is single-threaded, this would prevent all the other
                // tasks in the background from running.
                // In order to provide a better granularity, we force a yield after each new serie
                // of verifications.
                crate::yield_once().await;
            }

            // `sync_idle` is now an `Idle` that has been extracted from `sync`.
            // All the code paths below will need to put back `sync_idle` into `sync` before
            // looping again.

            // The sync state machine is idle, and all requests have been started.
            // Now waiting for some event to happen: a network event, a request from the frontend
            // of the sync service, or a request being finished.
            futures::select! {
                network_event = from_network_service.next() => {
                    // Something happened on the network.

                    let network_event = match network_event {
                        Some(m) => m,
                        None => {
                            // The channel from the network service has been closed. Closing the
                            // sync background task as well.
                            return
                        },
                    };

                    match network_event {
                        network_service::Event::Connected { peer_id, chain_index, best_block_number, best_block_hash }
                            if chain_index == network_chain_index =>
                        {
                            let (id, requests) = sync_idle.add_source(peer_id.clone(), best_block_number, best_block_hash);
                            peers_source_id_map.insert(peer_id, id);
                            requests_to_start.extend(requests);
                            sync = sync_idle.into();
                        },
                        network_service::Event::Disconnected { peer_id, chain_index }
                            if chain_index == network_chain_index =>
                        {
                            let id = peers_source_id_map.remove(&peer_id).unwrap();
                            let (rq_list, _) = sync_idle.remove_source(id);
                            for (rq_id, _) in rq_list {
                                pending_requests.remove(&rq_id).unwrap().abort();
                            }
                            sync = sync_idle.into();
                        },
                        network_service::Event::BlockAnnounce { chain_index, peer_id, announce }
                            if chain_index == network_chain_index =>
                        {
                            let id = *peers_source_id_map.get(&peer_id).unwrap();
                            let decoded = announce.decode();
                            // TODO: stupid to re-encode
                            match sync_idle.block_announce(id, decoded.header.scale_encoding_vec(), decoded.is_best) {
                                all::BlockAnnounceOutcome::HeaderVerify(verify) => {
                                    sync = verify.into();
                                },
                                all::BlockAnnounceOutcome::TooOld(idle) => {
                                    sync = idle.into();
                                },
                                all::BlockAnnounceOutcome::AlreadyInChain(idle) => {
                                    sync = idle.into();
                                },
                                all::BlockAnnounceOutcome::NotFinalizedChain(idle) => {
                                    sync = idle.into();
                                },
                                all::BlockAnnounceOutcome::Disjoint { sync: sync_idle, next_actions, .. } => {
                                    requests_to_start.extend(next_actions);
                                    sync = sync_idle.into();
                                },
                                all::BlockAnnounceOutcome::InvalidHeader { sync: sync_idle, .. } => {
                                    sync = sync_idle.into();
                                },
                            }
                        },
                        network_service::Event::GrandpaCommitMessage { chain_index, message }
                            if chain_index == network_chain_index =>
                        {
                            // TODO: verify the message and call `sync.set_finalized` or something
                            // TODO: has_new_finalized = true;
                            sync = sync_idle.into();
                        },
                        _ => {
                            // Different chain index.
                            sync = sync_idle.into();
                        }
                    }
                }

                message = from_foreground.next() => {
                    // Received message from the front `SyncService`.
                    let message = match message {
                        Some(m) => m,
                        None => {
                            // The channel with the frontend sync service has been closed.
                            // Closing the sync background task as a result.
                            return
                        },
                    };

                    match message {
                        ToBackground::IsNearHeadOfChainHeuristic { send_back } => {
                            let _ = send_back.send(sync_idle.is_near_head_of_chain_heuristic());
                        }
                        ToBackground::SubscribeFinalized { send_back } => {
                            let (tx, rx) = lossy_channel::channel();
                            finalized_notifications.push(tx);
                            let current = sync_idle.finalized_block_header().scale_encoding_vec();
                            let _ = send_back.send((current, rx));
                        }
                        ToBackground::SubscribeBest { send_back } => {
                            let (tx, rx) = lossy_channel::channel();
                            best_notifications.push(tx);
                            let current = sync_idle.best_block_header().scale_encoding_vec();
                            let _ = send_back.send((current, rx));
                        }
                    };

                    sync = sync_idle.into();
                },

                (request_id, result) = pending_block_requests.select_next_some() => {
                    pending_requests.remove(&request_id);

                    // A block(s) request has been finished.
                    // `result` is an error if the block request got cancelled by the sync state
                    // machine.
                    if let Ok(result) = result {
                        // Inject the result of the request into the sync state machine.
                        let outcome = sync_idle.blocks_request_response(
                            request_id,
                            result.map_err(|_| ()).map(|v| {
                                v.into_iter().filter_map(|block| {
                                    Some(all::BlockRequestSuccessBlock {
                                        scale_encoded_header: block.header?,
                                        scale_encoded_justification: block.justification,
                                        scale_encoded_extrinsics: Vec::new(),
                                        user_data: (),
                                    })
                                })
                            }),
                            ffi::unix_time(),
                        );

                        match outcome {
                            all::BlocksRequestResponseOutcome::VerifyHeader(verify) => {
                                sync = verify.into();
                            },
                            all::BlocksRequestResponseOutcome::Queued { sync: sync_idle, next_actions } => {
                                requests_to_start.extend(next_actions);
                                sync = sync_idle.into();
                            },
                            all::BlocksRequestResponseOutcome::NotFinalizedChain { sync: sync_idle, next_actions, .. } => {
                                requests_to_start.extend(next_actions);
                                sync = sync_idle.into();
                            },
                            all::BlocksRequestResponseOutcome::Inconclusive { sync: sync_idle, next_actions, .. } => {
                                requests_to_start.extend(next_actions);
                                sync = sync_idle.into();
                            },
                            all::BlocksRequestResponseOutcome::AllAlreadyInChain { sync: sync_idle, next_actions, .. } => {
                                requests_to_start.extend(next_actions);
                                sync = sync_idle.into();
                            },
                        }
                    } else {
                        // The sync state machine has emitted a `Action::Cancel` earlier, and is
                        // thus no longer interested in the response.
                        sync = sync_idle.into();
                    }
                },

                (request_id, result) = pending_grandpa_requests.select_next_some() => {
                    pending_requests.remove(&request_id);

                    // A GrandPa warp sync request has been finished.
                    // `result` is an error if the block request got cancelled by the sync state
                    // machine.
                    if let Ok(result) = result {
                        // Inject the result of the request into the sync state machine.
                        let outcome = sync_idle.grandpa_warp_sync_response(
                            request_id,
                            result.ok(),
                        );

                        match outcome {
                            all::GrandpaWarpSyncResponseOutcome::WarpSyncFinished {
                                sync: sync_idle, next_actions
                            } => {
                                let finalized_num = sync_idle.finalized_block_header().number;
                                log::info!(target: "sync-verify", "GrandPa warp sync finished to #{}", finalized_num);
                                has_new_finalized = true;
                                sync = sync_idle.into();
                                requests_to_start.extend(next_actions);
                            }
                            all::GrandpaWarpSyncResponseOutcome::Queued {
                                sync: sync_idle, next_actions
                            } => {
                                sync = sync_idle.into();
                                requests_to_start.extend(next_actions);
                            }
                        }

                    } else {
                        // The sync state machine has emitted a `Action::Cancel` earlier, and is
                        // thus no longer interested in the response.
                        sync = sync_idle.into();
                    }
                },

                (request_id, result) = pending_storage_requests.select_next_some() => {
                    pending_requests.remove(&request_id);

                    // A storage request has been finished.
                    // `result` is an error if the block request got cancelled by the sync state
                    // machine.
                    if let Ok(result) = result {
                        // Inject the result of the request into the sync state machine.
                        let outcome = sync_idle.storage_get_response(
                            request_id,
                            result.map(|list| list.into_iter()),
                        );

                        match outcome {
                            all::StorageGetResponseOutcome::WarpSyncFinished {
                                sync: sync_idle, next_actions
                            } => {
                                let finalized_num = sync_idle.finalized_block_header().number;
                                log::info!(target: "sync-verify", "GrandPa warp sync finished to #{}", finalized_num);
                                has_new_finalized = true;
                                sync = sync_idle.into();
                                requests_to_start.extend(next_actions);
                            }
                            all::StorageGetResponseOutcome::Queued {
                                sync: sync_idle, next_actions
                            } => {
                                sync = sync_idle.into();
                                requests_to_start.extend(next_actions);
                            }
                        }

                    } else {
                        // The sync state machine has emitted a `Action::Cancel` earlier, and is
                        // thus no longer interested in the response.
                        sync = sync_idle.into();
                    }
                },
            }
        }
    }
}

async fn start_parachain(
    chain_information: chain::chain_information::ChainInformation,
    mut from_foreground: mpsc::Receiver<ToBackground>,
    parachain_config: ConfigParachain,
) {
    // TODO: handle finality as well; this is semi-complicated because the runtime service needs to provide a way to call a function on the finalized block's runtime

    let relay_best_blocks = {
        let (relay_best_block_header, relay_best_blocks_subscription) =
            parachain_config.relay_chain_sync.subscribe_best().await;
        stream::once(future::ready(relay_best_block_header)).chain(relay_best_blocks_subscription)
    };
    futures::pin_mut!(relay_best_blocks);

    let mut current_finalized_block = chain_information.finalized_block_header;
    let mut current_best_block = current_finalized_block.clone();

    // List of senders that get notified when the best block is modified.
    let mut best_subscriptions = Vec::<lossy_channel::Sender<_>>::new();

    // Hash of the head data of the best block of the parachain. `None` if no head data obtained
    // yet. Used to avoid sending out notifications if the head data hasn't changed.
    let mut previous_best_head_data_hash = None::<[u8; 32]>;

    loop {
        futures::select! {
            message = from_foreground.next().fuse() => {
                // Terminating the parachain sync task if the foreground has closed.
                let message = match message {
                    Some(m) => m,
                    None => return,
                };

                // Note that the rest of this `select!` statement can block for a long time,
                // which means that there might be a big delay for processing the messages here.
                // At the time of writing, the nature of the messages makes this a non-issue,
                // but care should be taken about this.

                match message {
                    ToBackground::IsNearHeadOfChainHeuristic { send_back } => {
                        // TODO: that doesn't seem totally correct
                        let _ = send_back.send(previous_best_head_data_hash.is_some());
                    },
                    ToBackground::SubscribeFinalized { send_back } => {
                        let (tx, rx) = lossy_channel::channel();
                        core::mem::forget(tx); // TODO:
                        let _ = send_back.send((current_finalized_block.scale_encoding_vec(), rx));
                    }
                    ToBackground::SubscribeBest { send_back } => {
                        let (tx, rx) = lossy_channel::channel();
                        best_subscriptions.push(tx);
                        let _ = send_back.send((current_best_block.scale_encoding_vec(), rx));
                    }
                }
            },

            _relay_best_block = relay_best_blocks.next().fuse() => {
                // For each relay chain block, call `ParachainHost_persisted_validation_data` in
                // order to know where the parachains are.
                let pvd_result = parachain_config.relay_chain_sync.recent_best_block_runtime_call(
                    "ParachainHost_persisted_validation_data",
                    para::persisted_validation_data_parameters(
                        parachain_config.parachain_id,
                        para::OccupiedCoreAssumption::TimedOut
                    )
                ).await;

                // Even if there isn't any bug, the runtime call can likely fail because the relay
                // chain block has already been pruned from the network. This isn't a severe
                // error.
                let encoded_pvd = match pvd_result {
                    Ok(encoded_pvd) => encoded_pvd,
                    Err(err) => {
                        previous_best_head_data_hash = None;
                        if err.is_network_problem() {
                            log::debug!(target: "sync-verify", "Failed to get chain heads: {}", err);
                        } else {
                            log::warn!(target: "sync-verify", "Failed to get chain heads: {}", err);
                        }
                        continue;
                    }
                };

                // Try decode the result of the runtime call.
                // If this fails, it indicates an incompatibility between smoldot and the relay
                // chain.
                let head_data =
                    match para::decode_persisted_validation_data_return_value(&encoded_pvd) {
                        Ok(Some(pvd)) => pvd.parent_head,
                        Ok(None) => {
                            log::warn!(
                                target: "sync-verify",
                                "Couldn't find the parachain head from relay chain. \
                                The parachain likely doesn't occupy a core."
                            );
                            continue;
                        }
                        Err(error) => {
                            log::error!(
                                target: "sync-verify",
                                "Failed to fetch the parachain head from relay chain: {}",
                                error
                            );
                            continue;
                        }
                    };

                // Don't do anything more if the head data matches
                // `previous_best_head_data_hash`.
                match (&mut previous_best_head_data_hash, blake2b(32, &[], &head_data)) {
                    (&mut Some(ref mut h1), h2) if *h1 == h2.as_bytes() => continue,
                    (h1 @ _, h2) => *h1 = Some(<[u8; 32]>::try_from(h2.as_bytes()).unwrap()),
                };

                // The meaning of `head_data` depends on the parachain. It can represent
                // anything. In practice, however, it is most of the time a block header.
                match header::decode(&head_data) {
                    Ok(header) => {
                        current_best_block = header.into();
                        for sender in &mut best_subscriptions {
                            // TODO: remove senders if they're closed
                            let _ = sender.send(head_data.to_vec());
                        }
                    }
                    Err(_) => {
                        log::warn!(
                            target: "sync-verify",
                            "Head data is not a block header. This isn't supported by smoldot."
                        );
                    }
                }
            }
        }
    }
}

enum ToBackground {
    /// See [`SyncService::is_near_head_of_chain_heuristic`].
    IsNearHeadOfChainHeuristic { send_back: oneshot::Sender<bool> },
    /// See [`SyncService::subscribe_finalized`].
    SubscribeFinalized {
        send_back: oneshot::Sender<(Vec<u8>, lossy_channel::Receiver<Vec<u8>>)>,
    },
    /// See [`SyncService::subscribe_best`].
    SubscribeBest {
        send_back: oneshot::Sender<(Vec<u8>, lossy_channel::Receiver<Vec<u8>>)>,
    },
}
