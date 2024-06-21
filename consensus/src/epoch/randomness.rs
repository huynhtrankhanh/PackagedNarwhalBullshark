// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::consensus_manager::ConsensusClient;
use crate::messages_consensus::ConsensusTransaction;
use anemo::PeerId;
use fastcrypto::encoding::{Encoding, Hex};
use fastcrypto::error::FastCryptoError;
use fastcrypto::groups::bls12381;
use fastcrypto::serde_helpers::ToFromByteArray;
use fastcrypto::traits::{KeyPair, ToFromBytes};
use fastcrypto_tbls::nodes::PartyId;
use fastcrypto_tbls::{dkg, nodes};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use narwhal_types::{Round, TimestampMs};
use parking_lot::Mutex;
use rand::rngs::{OsRng, StdRng};
use rand::SeedableRng;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};
use std::time::Instant;
use sui_macros::fail_point_if;
use sui_network::randomness;
use sui_types::base_types::AuthorityName;
use sui_types::committee::{Committee, EpochId, StakeUnit};
use sui_types::crypto::{AuthorityKeyPair, RandomnessRound};
use sui_types::error::{SuiError, SuiResult};
use sui_types::sui_system_state::epoch_start_sui_system_state::EpochStartSystemStateTrait;
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use typed_store::rocks::DBBatch;
use typed_store::Map;

use crate::authority::authority_per_epoch_store::{AuthorityEpochTables, AuthorityPerEpochStore};
use crate::authority::epoch_start_configuration::EpochStartConfigTrait;
use crate::consensus_client::SubmitToConsensus;

type PkG = bls12381::G2Element;
type EncG = bls12381::G2Element;

pub const SINGLETON_KEY: u64 = 0;

// State machine for randomness DKG and generation.
//
// DKG protocol:
// 1. This validator sends out a `Message` to all other validators.
// 2. Once sufficient valid `Message`s are received from other validators via consensus and
//    procesed, this validator sends out a `Confirmation` to all other validators.
// 3. Once sufficient `Confirmation`s are received from other validators via consensus and
//    processed, they are combined to form a public VSS key and local private key shares.
// 4. Randomness generation begins.
//
// Randomness generation:
// 1. For each new round, AuthorityPerEpochStore eventually calls `generate_randomness`.
// 2. This kicks off a process in RandomnessEventLoop to send partial signatures for the new
//    round to all other validators.
// 3. Once enough partial signautres for the round are collected, a RandomnessStateUpdate
//    transaction is generated and injected into the TransactionManager.
// 4. Once the RandomnessStateUpdate transaction is seen in a certified checkpoint,
//    `notify_randomness_in_checkpoint` is called to complete the round and stop sending
//    partial signatures for it.
pub struct RandomnessManager {
    epoch_store: Weak<AuthorityPerEpochStore>,
    epoch: EpochId,
    consensus_client: Arc<ConsensusClient>,
    network_handle: randomness::Handle,
    authority_info: HashMap<AuthorityName, (PeerId, PartyId)>,

    // State for DKG.
    dkg_start_time: OnceCell<Instant>,
    party: Arc<dkg::Party<PkG, EncG>>,
    enqueued_messages: BTreeMap<PartyId, JoinHandle<Option<dkg::ProcessedMessage<PkG, EncG>>>>,
    processed_messages: BTreeMap<PartyId, dkg::ProcessedMessage<PkG, EncG>>,
    used_messages: OnceCell<dkg::UsedProcessedMessages<PkG, EncG>>,
    confirmations: BTreeMap<PartyId, dkg::Confirmation<EncG>>,
    dkg_output: OnceCell<Option<dkg::Output<PkG, EncG>>>,

    // State for randomness generation.
    next_randomness_round: RandomnessRound,
    highest_completed_round: Arc<Mutex<RandomnessRound>>,
}

impl RandomnessManager {
    // Returns None in case of invalid input or other failure to initialize DKG.
    pub async fn try_new(
        epoch_store_weak: Weak<AuthorityPerEpochStore>,
        consensus_client: Arc<ConsensusClient>,
        network_handle: randomness::Handle,
        authority_key_pair: &AuthorityKeyPair,
    ) -> Option<Self> {
        let epoch_store = match epoch_store_weak.upgrade() {
            Some(epoch_store) => epoch_store,
            None => {
                error!(
                    "could not construct RandomnessManager: AuthorityPerEpochStore already gone"
                );
                return None;
            }
        };
        let tables = match epoch_store.tables() {
            Ok(tables) => tables,
            Err(_) => {
                error!("could not construct RandomnessManager: AuthorityPerEpochStore tables already gone");
                return None;
            }
        };
        let protocol_config = epoch_store.protocol_config();

        let name: AuthorityName = authority_key_pair.public().into();
        let committee = epoch_store.committee();
        let info = RandomnessManager::randomness_dkg_info_from_committee(committee);
        if tracing::enabled!(tracing::Level::DEBUG) {
            // Log first few entries in DKG info for debugging.
            for (id, name, pk, stake) in info.iter().filter(|(id, _, _, _)| *id < 3) {
                let pk_bytes = pk.as_element().to_byte_array();
                debug!("random beacon: DKG info: id={id}, stake={stake}, name={name}, pk={pk_bytes:x?}");
            }
        }
        let authority_ids: HashMap<_, _> =
            info.iter().map(|(id, name, _, _)| (*name, *id)).collect();
        let authority_peer_ids = epoch_store
            .epoch_start_config()
            .epoch_start_state()
            .get_authority_names_to_peer_ids();
        let authority_info = authority_ids
            .into_iter()
            .map(|(name, id)| {
                let peer_id = *authority_peer_ids
                    .get(&name)
                    .expect("authority name should be in peer_ids");
                (name, (peer_id, id))
            })
            .collect();
        let nodes = info
            .iter()
            .map(|(id, _, pk, stake)| nodes::Node::<EncG> {
                id: *id,
                pk: pk.clone(),
                weight: (*stake).try_into().expect("stake should fit in u16"),
            })
            .collect();
        let (nodes, t) = match nodes::Nodes::new_reduced(
            nodes,
            committee
                .validity_threshold()
                .try_into()
                .expect("validity threshold should fit in u16"),
            protocol_config.random_beacon_reduction_allowed_delta(),
            protocol_config
                .random_beacon_reduction_lower_bound()
                .try_into()
                .expect("should fit u16"),
        ) {
            Ok((nodes, t)) => (nodes, t),
            Err(err) => {
                error!("random beacon: error while initializing Nodes: {err:?}");
                return None;
            }
        };
        let total_weight = nodes.total_weight();
        let num_nodes = nodes.num_nodes();
        let prefix_str = format!(
            "dkg {} {}",
            Hex::encode(epoch_store.get_chain_identifier().as_bytes()),
            committee.epoch()
        );
        let randomness_private_key = bls12381::Scalar::from_byte_array(
            authority_key_pair
                .copy()
                .private()
                .as_bytes()
                .try_into()
                .expect("key length should match"),
        )
        .expect("should work to convert BLS key to Scalar");
        let party = match dkg::Party::<PkG, EncG>::new(
            fastcrypto_tbls::ecies::PrivateKey::<bls12381::G2Element>::from(randomness_private_key),
            nodes,
            t,
            fastcrypto_tbls::random_oracle::RandomOracle::new(prefix_str.as_str()),
            &mut rand::thread_rng(),
        ) {
            Ok(party) => party,
            Err(err) => {
                error!("random beacon: error while initializing Party: {err:?}");
                return None;
            }
        };
        info!(
            "random beacon: state initialized with authority={name}, total_weight={total_weight}, t={t}, num_nodes={num_nodes}, oracle initial_prefix={prefix_str:?}",
        );

        // Load existing data from store.
        let highest_completed_round = tables
            .randomness_highest_completed_round
            .get(&SINGLETON_KEY)
            .expect("typed_store should not fail")
            .unwrap_or(RandomnessRound(0));
        let mut rm = RandomnessManager {
            epoch_store: epoch_store_weak,
            epoch: committee.epoch(),
            consensus_client,
            network_handle: network_handle.clone(),
            authority_info,
            dkg_start_time: OnceCell::new(),
            party: Arc::new(party),
            enqueued_messages: BTreeMap::new(),
            processed_messages: BTreeMap::new(),
            used_messages: OnceCell::new(),
            confirmations: BTreeMap::new(),
            dkg_output: OnceCell::new(),
            next_randomness_round: RandomnessRound(0),
            highest_completed_round: Arc::new(Mutex::new(highest_completed_round)),
        };
        let dkg_output = tables
            .dkg_output
            .get(&SINGLETON_KEY)
            .expect("typed_store should not fail");
        if let Some(dkg_output) = dkg_output {
            info!(
                "random beacon: loaded existing DKG output for epoch {}",
                committee.epoch()
            );
            epoch_store
                .metrics
                .epoch_random_beacon_dkg_num_shares
                .set(dkg_output.shares.as_ref().map_or(0, |shares| shares.len()) as i64);
            rm.dkg_output
                .set(Some(dkg_output.clone()))
                .expect("setting new OnceCell should succeed");
            network_handle.update_epoch(
                committee.epoch(),
                rm.authority_info.clone(),
                dkg_output,
                rm.party.t(),
                Some(highest_completed_round),
            );
        } else {
            info!(
                "random beacon: no existing DKG output found for epoch {}",
                committee.epoch()
            );
            // Load intermediate data.
            rm.processed_messages.extend(
                tables
                    .dkg_processed_messages
                    .safe_iter()
                    .map(|result| result.expect("typed_store should not fail")),
            );
            if let Some(used_messages) = tables
                .dkg_used_messages
                .get(&SINGLETON_KEY)
                .expect("typed_store should not fail")
            {
                rm.used_messages
                    .set(used_messages.clone())
                    .expect("setting new OnceCell should succeed");
            }
            rm.confirmations.extend(
                tables
                    .dkg_confirmations
                    .safe_iter()
                    .map(|result| result.expect("typed_store should not fail")),
            );
        }

        // Resume randomness generation from where we left off.
        // This must be loaded regardless of whether DKG has finished yet, since the
        // RandomnessEventLoop and commit-handling logic in AuthorityPerEpochStore both depend on
        // this state.
        rm.next_randomness_round = tables
            .randomness_next_round
            .get(&SINGLETON_KEY)
            .expect("typed_store should not fail")
            .unwrap_or(RandomnessRound(0));
        info!(
            "random beacon: starting from next_randomness_round={}",
            rm.next_randomness_round.0
        );
        for result in tables.randomness_rounds_pending.safe_iter() {
            let (round, _) = result.expect("typed_store should not fail");
            info!(
                "random beacon: resuming generation for randomness round {}",
                round.0
            );
            network_handle.send_partial_signatures(committee.epoch(), round);
        }

        Some(rm)
    }

    /// Sends the initial dkg::Message to begin the randomness DKG protocol.
    pub async fn start_dkg(&mut self) -> SuiResult {
        if self.used_messages.initialized() || self.dkg_output.initialized() {
            // DKG already started (or completed or failed).
            return Ok(());
        }
        let _ = self.dkg_start_time.set(Instant::now());

        let msg = match self.party.create_message(&mut rand::thread_rng()) {
            Ok(msg) => msg,
            Err(FastCryptoError::IgnoredMessage) => {
                info!(
                    "random beacon: no DKG Message for party id={} (zero weight)",
                    self.party.id
                );
                return Ok(());
            }
            Err(e) => {
                error!("random beacon: error while creating a DKG Message: {e:?}");
                return Ok(());
            }
        };

        info!(
            "random beacon: created DKG Message with sender={}, vss_pk.degree={}, encrypted_shares.len()={}",
            msg.sender,
            msg.vss_pk.degree(),
            msg.encrypted_shares.len(),
        );

        let epoch_store = self.epoch_store()?;
        let transaction = ConsensusTransaction::new_randomness_dkg_message(epoch_store.name, &msg);

        #[allow(unused_mut)]
        let mut fail_point_skip_sending = false;
        fail_point_if!("rb-dkg", || {
            // maybe skip sending in simtests
            fail_point_skip_sending = true;
        });
        if !fail_point_skip_sending {
            self.consensus_client
                .submit_consensus_transaction(transaction)
                .await?;
        }

        epoch_store
            .metrics
            .epoch_random_beacon_dkg_message_time_ms
            .set(
                self.dkg_start_time
                    .get()
                    .unwrap() // already set above
                    .elapsed()
                    .as_millis() as i64,
            );
        Ok(())
    }

    /// Processes all received messages and advances the randomness DKG state machine when possible,
    /// sending out a dkg::Confirmation and generating final output.
    pub async fn advance_dkg(&mut self, batch: &mut DBBatch, round: Round) -> SuiResult {
        let epoch_store = self.epoch_store()?;

        // Once we have enough Messages, send a Confirmation.
        if !self.dkg_output.initialized() && !self.used_messages.initialized() {
            // Process all enqueued messages.
            let mut handles: FuturesUnordered<_> = std::mem::take(&mut self.enqueued_messages)
                .into_values()
                .collect();
            while let Some(res) = handles.next().await {
                if let Ok(Some(processed)) = res {
                    self.processed_messages
                        .insert(processed.message.sender, processed.clone());
                    batch.insert_batch(
                        &epoch_store.tables()?.dkg_processed_messages,
                        std::iter::once((processed.message.sender, processed)),
                    )?;
                }
            }

            // Attempt to generate the Confirmation.
            match self.party.merge(
                &self
                    .processed_messages
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
            ) {
                Ok((conf, used_msgs)) => {
                    info!(
                        "random beacon: sending DKG Confirmation with {} complaints",
                        conf.complaints.len()
                    );
                    if self.used_messages.set(used_msgs.clone()).is_err() {
                        error!("BUG: used_messages should only ever be set once");
                    }
                    batch.insert_batch(
                        &epoch_store.tables()?.dkg_used_messages,
                        std::iter::once((SINGLETON_KEY, used_msgs)),
                    )?;

                    let transaction = ConsensusTransaction::new_randomness_dkg_confirmation(
                        epoch_store.name,
                        &conf,
                    );

                    #[allow(unused_mut)]
                    let mut fail_point_skip_sending = false;
                    fail_point_if!("rb-dkg", || {
                        // maybe skip sending in simtests
                        fail_point_skip_sending = true;
                    });
                    if !fail_point_skip_sending {
                        self.consensus_client
                            .submit_consensus_transaction(transaction)
                            .await?;
                    }

                    let elapsed = self.dkg_start_time.get().map(|t| t.elapsed().as_millis());
                    if let Some(elapsed) = elapsed {
                        epoch_store
                            .metrics
                            .epoch_random_beacon_dkg_confirmation_time_ms
                            .set(elapsed as i64);
                    }
                }
                Err(fastcrypto::error::FastCryptoError::NotEnoughInputs) => (), // wait for more input
                Err(e) => debug!("random beacon: error while merging DKG Messages: {e:?}"),
            }
        }

        // Once we have enough Confirmations, process them and update shares.
        if !self.dkg_output.initialized() && self.used_messages.initialized() {
            match self.party.complete(
                self.used_messages
                    .get()
                    .expect("checked above that `used_messages` is initialized"),
                &self.confirmations.values().cloned().collect::<Vec<_>>(),
                &mut StdRng::from_rng(OsRng).expect("RNG construction should not fail"),
            ) {
                Ok(output) => {
                    let num_shares = output.shares.as_ref().map_or(0, |shares| shares.len());
                    let epoch_elapsed = epoch_store.epoch_open_time.elapsed().as_millis();
                    let elapsed = self.dkg_start_time.get().map(|t| t.elapsed().as_millis());
                    info!("random beacon: DKG complete in {epoch_elapsed}ms since epoch start, {elapsed:?}ms since DKG start, with {num_shares} shares for this node");
                    epoch_store
                        .metrics
                        .epoch_random_beacon_dkg_num_shares
                        .set(num_shares as i64);
                    epoch_store
                        .metrics
                        .epoch_random_beacon_dkg_epoch_start_completion_time_ms
                        .set(epoch_elapsed as i64);
                    epoch_store.metrics.epoch_random_beacon_dkg_failed.set(0);
                    if let Some(elapsed) = elapsed {
                        epoch_store
                            .metrics
                            .epoch_random_beacon_dkg_completion_time_ms
                            .set(elapsed as i64);
                    }
                    self.dkg_output
                        .set(Some(output.clone()))
                        .expect("checked above that `dkg_output` is uninitialized");
                    self.network_handle.update_epoch(
                        epoch_store.committee().epoch(),
                        self.authority_info.clone(),
                        output.clone(),
                        self.party.t(),
                        None,
                    );
                    batch.insert_batch(
                        &epoch_store.tables()?.dkg_output,
                        std::iter::once((SINGLETON_KEY, output)),
                    )?;
                }
                Err(fastcrypto::error::FastCryptoError::NotEnoughInputs) => (), // wait for more input
                Err(e) => error!("random beacon: error while processing DKG Confirmations: {e:?}"),
            }
        }

        // If we ran out of time, mark DKG as failed.
        if !self.dkg_output.initialized()
            && round
                > epoch_store
                    .protocol_config()
                    .random_beacon_dkg_timeout_round()
                    .into()
        {
            error!("random beacon: DKG timed out. Randomness disabled for this epoch. All randomness-using transactions will fail.");
            epoch_store.metrics.epoch_random_beacon_dkg_failed.set(1);
            self.dkg_output
                .set(None)
                .expect("checked above that `dkg_output` is uninitialized");
        }

        Ok(())
    }

    /// Adds a received dkg::Message to the randomness DKG state machine.
    pub fn add_message(
        &mut self,
        authority: &AuthorityName,
        msg: dkg::Message<PkG, EncG>,
    ) -> SuiResult {
        if self.used_messages.initialized() || self.dkg_output.initialized() {
            // We've already sent a `Confirmation`, so we can't add any more messages.
            return Ok(());
        }
        let Some((_, party_id)) = self.authority_info.get(authority) else {
            error!("random beacon: received DKG Message from unknown authority: {authority:?}");
            return Ok(());
        };
        if *party_id != msg.sender {
            warn!("ignoring equivocating DKG Message from authority {authority:?} pretending to be PartyId {party_id:?}");
            return Ok(());
        }
        if self.enqueued_messages.contains_key(&msg.sender)
            || self.processed_messages.contains_key(&msg.sender)
        {
            info!("ignoring duplicate DKG Message from authority {authority:?}");
            return Ok(());
        }

        let party = self.party.clone();
        // TODO: Could save some CPU by not processing messages if we already have enough to merge.
        self.enqueued_messages.insert(
            msg.sender,
            tokio::task::spawn_blocking(move || {
                match party.process_message(msg, &mut rand::thread_rng()) {
                    Ok(processed) => Some(processed),
                    Err(err) => {
                        debug!("random beacon: error while processing DKG Message: {err:?}");
                        None
                    }
                }
            }),
        );
        Ok(())
    }

    /// Adds a received dkg::Confirmation to the randomness DKG state machine.
    pub fn add_confirmation(
        &mut self,
        batch: &mut DBBatch,
        authority: &AuthorityName,
        conf: dkg::Confirmation<EncG>,
    ) -> SuiResult {
        if self.dkg_output.initialized() {
            // Once we have completed DKG, no more `Confirmation`s are needed.
            return Ok(());
        }
        let Some((_, party_id)) = self.authority_info.get(authority) else {
            error!(
                "random beacon: received DKG Confirmation from unknown authority: {authority:?}"
            );
            return Ok(());
        };
        if *party_id != conf.sender {
            warn!("ignoring equivocating DKG Confirmation from authority {authority:?} pretending to be PartyId {party_id:?}");
            return Ok(());
        }
        self.confirmations.insert(conf.sender, conf.clone());
        batch.insert_batch(
            &self.tables()?.dkg_confirmations,
            std::iter::once((conf.sender, conf)),
        )?;
        Ok(())
    }

    /// Reserves the next available round number for randomness generation if enough time has
    /// elapsed, or returns None if not yet ready (based on ProtocolConfig setting). Once the given
    /// batch is written, `generate_randomness` must be called to start the process. On restart,
    /// any reserved rounds for which the batch was written will automatically be resumed.
    pub fn reserve_next_randomness(
        &mut self,
        commit_timestamp: TimestampMs,
        batch: &mut DBBatch,
    ) -> SuiResult<Option<RandomnessRound>> {
        let epoch_store = self.epoch_store()?;
        let tables = epoch_store.tables()?;

        let last_round_timestamp = tables
            .randomness_last_round_timestamp
            .get(&SINGLETON_KEY)
            .expect("typed_store should not fail");
        if let Some(last_round_timestamp) = last_round_timestamp {
            if commit_timestamp - last_round_timestamp
                < epoch_store
                    .protocol_config()
                    .random_beacon_min_round_interval_ms()
            {
                return Ok(None);
            }
        }

        let randomness_round = self.next_randomness_round;
        self.next_randomness_round = self
            .next_randomness_round
            .checked_add(1)
            .expect("RandomnessRound should not overflow");

        batch.insert_batch(
            &tables.randomness_rounds_pending,
            std::iter::once((randomness_round, ())),
        )?;
        batch.insert_batch(
            &tables.randomness_next_round,
            std::iter::once((SINGLETON_KEY, self.next_randomness_round)),
        )?;
        batch.insert_batch(
            &tables.randomness_last_round_timestamp,
            std::iter::once((SINGLETON_KEY, commit_timestamp)),
        )?;

        Ok(Some(randomness_round))
    }

    /// Starts the process of generating the given RandomnessRound.
    pub fn generate_randomness(&self, epoch: EpochId, randomness_round: RandomnessRound) {
        self.network_handle
            .send_partial_signatures(epoch, randomness_round);
    }

    pub fn dkg_status(&self) -> DkgStatus {
        match self.dkg_output.get() {
            Some(Some(_)) => DkgStatus::Successful,
            Some(None) => DkgStatus::Failed,
            None => DkgStatus::Pending,
        }
    }

    /// Generates a new RandomnessReporter for reporting observed rounds to this RandomnessManager.
    pub fn reporter(&self) -> RandomnessReporter {
        RandomnessReporter {
            epoch_store: self.epoch_store.clone(),
            epoch: self.epoch,
            network_handle: self.network_handle.clone(),
            highest_completed_round: self.highest_completed_round.clone(),
        }
    }

    fn epoch_store(&self) -> SuiResult<Arc<AuthorityPerEpochStore>> {
        self.epoch_store
            .upgrade()
            .ok_or(SuiError::EpochEnded(self.epoch))
    }

    fn tables(&self) -> SuiResult<Arc<AuthorityEpochTables>> {
        self.epoch_store()?.tables()
    }

    fn randomness_dkg_info_from_committee(
        committee: &Committee,
    ) -> Vec<(
        u16,
        AuthorityName,
        fastcrypto_tbls::ecies::PublicKey<bls12381::G2Element>,
        StakeUnit,
    )> {
        committee
            .members()
            .map(|(name, stake)| {
                let index: u16 = committee
                    .authority_index(name)
                    .expect("lookup of known committee member should succeed")
                    .try_into()
                    .expect("authority index should fit in u16");
                let pk = bls12381::G2Element::from_byte_array(
                    committee
                        .public_key(name)
                        .expect("lookup of known committee member should succeed")
                        .as_bytes()
                        .try_into()
                        .expect("key length should match"),
                )
                .expect("should work to convert BLS key to G2Element");
                (
                    index,
                    *name,
                    fastcrypto_tbls::ecies::PublicKey::from(pk),
                    *stake,
                )
            })
            .collect()
    }
}

// Used by other components to notify the randomness system of observed randomness.
#[derive(Clone)]
pub struct RandomnessReporter {
    epoch_store: Weak<AuthorityPerEpochStore>,
    epoch: EpochId,
    network_handle: randomness::Handle,
    highest_completed_round: Arc<Mutex<RandomnessRound>>,
}

impl RandomnessReporter {
    /// Notifies the associated randomness manager that randomness for the given round has been
    /// durably committed in a checkpoint. This completes the process of generating randomness for
    /// the round.
    pub fn notify_randomness_in_checkpoint(&self, round: RandomnessRound) -> SuiResult {
        let epoch_store = self
            .epoch_store
            .upgrade()
            .ok_or(SuiError::EpochEnded(self.epoch))?;
        epoch_store
            .tables()?
            .randomness_rounds_pending
            .remove(&round)?;
        let mut highest_completed_round = self.highest_completed_round.lock();
        if round > *highest_completed_round {
            *highest_completed_round = round;
            epoch_store
                .tables()?
                .randomness_highest_completed_round
                .insert(&SINGLETON_KEY, &highest_completed_round)?;
        }
        self.network_handle
            .complete_round(epoch_store.committee().epoch(), round);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DkgStatus {
    Pending,
    Failed,
    Successful,
}

#[cfg(test)]
mod tests {
    use crate::{
        authority::test_authority_builder::TestAuthorityBuilder,
        // consensus_adapter::{
        //     ConnectionMonitorStatusForTests, ConsensusAdapter, ConsensusAdapterMetrics,
        //     MockSubmitToConsensus,
        // },
        epoch::randomness::*,
    };
    use std::num::NonZeroUsize;
    use sui_types::messages_consensus::ConsensusTransactionKind;
    use tokio::sync::mpsc;

    // #[tokio::test]
    // async fn test_dkg() {
    //     telemetry_subscribers::init_for_testing();

    //     let network_config =
    //         sui_swarm_config::network_config_builder::ConfigBuilder::new_with_temp_dir()
    //             .committee_size(NonZeroUsize::new(4).unwrap())
    //             .with_reference_gas_price(500)
    //             .build();

    //     let mut epoch_stores = Vec::new();
    //     let mut randomness_managers = Vec::new();
    //     let (tx_consensus, mut rx_consensus) = mpsc::channel(100);

    //     for validator in network_config.validator_configs.iter() {
    //         // Send consensus messages to channel.
    //         let mut mock_consensus_client = MockSubmitToConsensus::new();
    //         let tx_consensus = tx_consensus.clone();
    //         mock_consensus_client
    //             .expect_submit_to_consensus()
    //             .withf(move |transactions: &[ConsensusTransaction]| {
    //                 tx_consensus.try_send(transactions.to_vec()).unwrap();
    //                 true
    //             })
    //             .returning(|_| Ok(()));

    //         let state = TestAuthorityBuilder::new()
    //             .with_genesis_and_keypair(&network_config.genesis, validator.protocol_key_pair())
    //             .build()
    //             .await;
    //         // let consensus_adapter = Arc::new(ConsensusAdapter::new(
    //         //     Arc::new(mock_consensus_client),
    //         //     state.name,
    //         //     Arc::new(ConnectionMonitorStatusForTests {}),
    //         //     100_000,
    //         //     100_000,
    //         //     None,
    //         //     None,
    //         //     ConsensusAdapterMetrics::new_test(),
    //         //     state.epoch_store_for_testing().protocol_config().clone(),
    //         // ));
    //         let epoch_store = state.epoch_store_for_testing();
    //         let randomness_manager = RandomnessManager::try_new(
    //             Arc::downgrade(&epoch_store),
    //             consensus_client.clone(),
    //             sui_network::randomness::Handle::new_stub(),
    //             validator.protocol_key_pair(),
    //         )
    //         .await
    //         .unwrap();

    //         epoch_stores.push(epoch_store);
    //         randomness_managers.push(randomness_manager);
    //     }

    //     // Generate and distribute Messages.
    //     let mut dkg_messages = Vec::new();
    //     for randomness_manager in randomness_managers.iter_mut() {
    //         randomness_manager.start_dkg().unwrap();

    //         let mut dkg_message = rx_consensus.recv().await.unwrap();
    //         assert!(dkg_message.len() == 1);
    //         match dkg_message.remove(0).kind {
    //             ConsensusTransactionKind::RandomnessDkgMessage(_, bytes) => {
    //                 let msg: fastcrypto_tbls::dkg::Message<PkG, EncG> = bcs::from_bytes(&bytes)
    //                     .expect("DKG message deserialization should not fail");
    //                 dkg_messages.push(msg);
    //             }
    //             _ => panic!("wrong type of message sent"),
    //         }
    //     }
    //     for i in 0..randomness_managers.len() {
    //         let mut batch = epoch_stores[i]
    //             .tables()
    //             .unwrap()
    //             .dkg_processed_messages
    //             .batch();
    //         for (j, dkg_message) in dkg_messages.iter().cloned().enumerate() {
    //             randomness_managers[i]
    //                 .add_message(&epoch_stores[j].name, dkg_message)
    //                 .unwrap();
    //         }
    //         randomness_managers[i]
    //             .advance_dkg(&mut batch, 0)
    //             .await
    //             .unwrap();
    //         batch.write().unwrap();
    //     }

    //     // Generate and distribute Confirmations.
    //     let mut dkg_confirmations = Vec::new();
    //     for _ in 0..randomness_managers.len() {
    //         let mut dkg_confirmation = rx_consensus.recv().await.unwrap();
    //         assert!(dkg_confirmation.len() == 1);
    //         match dkg_confirmation.remove(0).kind {
    //             ConsensusTransactionKind::RandomnessDkgConfirmation(_, bytes) => {
    //                 let msg: fastcrypto_tbls::dkg::Confirmation<EncG> = bcs::from_bytes(&bytes)
    //                     .expect("DKG confirmation deserialization should not fail");
    //                 dkg_confirmations.push(msg);
    //             }
    //             _ => panic!("wrong type of message sent"),
    //         }
    //     }
    //     for i in 0..randomness_managers.len() {
    //         let mut batch = epoch_stores[i].tables().unwrap().dkg_confirmations.batch();
    //         for (j, dkg_confirmation) in dkg_confirmations.iter().cloned().enumerate() {
    //             randomness_managers[i]
    //                 .add_confirmation(&mut batch, &epoch_stores[j].name, dkg_confirmation)
    //                 .unwrap();
    //         }
    //         randomness_managers[i]
    //             .advance_dkg(&mut batch, 0)
    //             .await
    //             .unwrap();
    //         batch.write().unwrap();
    //     }

    //     // Verify DKG completed.
    //     for randomness_manager in &randomness_managers {
    //         assert_eq!(DkgStatus::Successful, randomness_manager.dkg_status());
    //     }
    // }

    // #[tokio::test]
    // async fn test_dkg_expiration() {
    //     telemetry_subscribers::init_for_testing();

    //     let network_config =
    //         sui_swarm_config::network_config_builder::ConfigBuilder::new_with_temp_dir()
    //             .committee_size(NonZeroUsize::new(4).unwrap())
    //             .with_reference_gas_price(500)
    //             .build();

    //     let mut epoch_stores = Vec::new();
    //     let mut randomness_managers = Vec::new();
    //     let (tx_consensus, mut rx_consensus) = mpsc::channel(100);

    //     for validator in network_config.validator_configs.iter() {
    //         // Send consensus messages to channel.
    //         let mut mock_consensus_client = MockSubmitToConsensus::new();
    //         let tx_consensus = tx_consensus.clone();
    //         mock_consensus_client
    //             .expect_submit_to_consensus()
    //             .withf(move |transactions: &[ConsensusTransaction], _epoch_store| {
    //                 tx_consensus.try_send(transactions.to_vec()).unwrap();
    //                 true
    //             })
    //             .returning(|_, _| Ok(()));

    //         let state = TestAuthorityBuilder::new()
    //             .with_genesis_and_keypair(&network_config.genesis, validator.protocol_key_pair())
    //             .build()
    //             .await;
    //         let consensus_adapter = Arc::new(ConsensusAdapter::new(
    //             Arc::new(mock_consensus_client),
    //             state.name,
    //             Arc::new(ConnectionMonitorStatusForTests {}),
    //             100_000,
    //             100_000,
    //             None,
    //             None,
    //             ConsensusAdapterMetrics::new_test(),
    //             state.epoch_store_for_testing().protocol_config().clone(),
    //         ));
    //         let epoch_store = state.epoch_store_for_testing();
    //         let randomness_manager = RandomnessManager::try_new(
    //             Arc::downgrade(&epoch_store),
    //             consensus_adapter.clone(),
    //             sui_network::randomness::Handle::new_stub(),
    //             validator.protocol_key_pair(),
    //         )
    //         .await
    //         .unwrap();

    //         epoch_stores.push(epoch_store);
    //         randomness_managers.push(randomness_manager);
    //     }

    //     // Generate and distribute Messages.
    //     let mut dkg_messages = Vec::new();
    //     for randomness_manager in randomness_managers.iter_mut() {
    //         randomness_manager.start_dkg().unwrap();

    //         let mut dkg_message = rx_consensus.recv().await.unwrap();
    //         assert!(dkg_message.len() == 1);
    //         match dkg_message.remove(0).kind {
    //             ConsensusTransactionKind::RandomnessDkgMessage(_, bytes) => {
    //                 let msg: fastcrypto_tbls::dkg::Message<PkG, EncG> = bcs::from_bytes(&bytes)
    //                     .expect("DKG message deserialization should not fail");
    //                 dkg_messages.push(msg);
    //             }
    //             _ => panic!("wrong type of message sent"),
    //         }
    //     }
    //     for i in 0..randomness_managers.len() {
    //         let mut batch = epoch_stores[i]
    //             .tables()
    //             .unwrap()
    //             .dkg_processed_messages
    //             .batch();
    //         for (j, dkg_message) in dkg_messages.iter().cloned().enumerate() {
    //             randomness_managers[i]
    //                 .add_message(&epoch_stores[j].name, dkg_message)
    //                 .unwrap();
    //         }
    //         randomness_managers[i]
    //             .advance_dkg(&mut batch, u64::MAX)
    //             .await
    //             .unwrap();
    //         batch.write().unwrap();
    //     }

    //     // Verify DKG failed.
    //     for randomness_manager in &randomness_managers {
    //         assert_eq!(DkgStatus::Failed, randomness_manager.dkg_status());
    //     }
    // }
}
