//! Vote and vote accumulator types
//!
//! This module contains types used to represent the various types of votes that `HotShot` nodes
//! can send, and vote accumulator that converts votes into certificates.

use crate::{
    certificate::{AssembledSignature, QuorumCertificate},
    data::LeafType,
    traits::{
        election::{VoteData, VoteToken},
        node_implementation::NodeType,
        signature_key::{EncodedPublicKey, EncodedSignature, SignatureKey},
    },
};
use bincode::Options;
use bitvec::prelude::*;
use commit::{Commitment, Committable};
use either::Either;
use ethereum_types::U256;
use hotshot_utils::bincode::bincode_opts;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    marker::PhantomData,
    num::NonZeroU64,
};
use tracing::error;

/// The vote sent by consensus messages.
pub trait VoteType<TYPES: NodeType, COMMITMENT: for<'a> Deserialize<'a> + Serialize + Clone>:
    Debug + Clone + 'static + Serialize + for<'a> Deserialize<'a> + Send + Sync + PartialEq
{
    /// Get the view this vote was cast for
    fn get_view(&self) -> TYPES::Time;
    /// Get the signature key associated with this vote
    fn get_key(&self) -> TYPES::SignatureKey;
    /// Get the signature associated with this vote
    fn get_signature(&self) -> EncodedSignature;
    /// Get the data this vote was signed over
    fn get_data(&self) -> VoteData<COMMITMENT>;
    /// Get the vote token of this vote
    fn get_vote_token(&self) -> TYPES::VoteTokenType;
}

/// A vote on DA proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Hash, Eq)]
#[serde(bound(deserialize = ""))]
pub struct DAVote<TYPES: NodeType> {
    /// The signature share associated with this vote
    pub signature: (EncodedPublicKey, EncodedSignature),
    /// The block commitment being voted on.
    pub block_commitment: Commitment<TYPES::BlockType>,
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<Commitment<TYPES::BlockType>>,
}

/// A positive or negative vote on validating or commitment proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
pub struct YesOrNoVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// TODO we should remove this
    /// this is correct, but highly inefficient
    /// we should check a cache, and if that fails request the qc
    pub justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
    /// The signature share associated with this vote
    pub signature: (EncodedPublicKey, EncodedSignature),
    /// The leaf commitment being voted on.
    pub leaf_commitment: Commitment<LEAF>,
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<Commitment<LEAF>>,
}

/// A timeout vote.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
pub struct TimeoutVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// The highest valid QC this node knows about
    pub high_qc: QuorumCertificate<TYPES, LEAF>,
    /// The signature share associated with this vote
    pub signature: (EncodedPublicKey, EncodedSignature),
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<Commitment<TYPES::Time>>,
}

/// The internals of a view sync vote
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
pub struct ViewSyncVoteInternal<TYPES: NodeType> {
    /// The public key associated with the relay.
    pub relay_pub_key: EncodedPublicKey,
    /// The relay this vote is intended for
    pub relay: u64,
    /// The view number we are trying to sync on
    pub round: TYPES::Time,
    /// This node's signature over the VoteData
    pub signature: (EncodedPublicKey, EncodedSignature),
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<Commitment<ViewSyncData<TYPES>>>,
}

/// The data View Sync votes are signed over
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Hash, Eq)]
#[serde(bound(deserialize = ""))]
pub struct ViewSyncData<TYPES: NodeType> {
    /// The relay this vote is intended for
    pub relay: EncodedPublicKey,
    /// The view number we are trying to sync on
    pub round: TYPES::Time,
}

impl<TYPES: NodeType> Committable for ViewSyncData<TYPES> {
    fn commit(&self) -> Commitment<Self> {
        let builder = commit::RawCommitmentBuilder::new("Quorum Certificate Commitment");

        builder
            .var_size_field("Relay public key", &self.relay.0)
            .u64(*self.round)
            .finalize()
    }
}

/// Votes to synchronize the network on a single view
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
pub enum ViewSyncVote<TYPES: NodeType> {
    /// PreCommit vote
    PreCommit(ViewSyncVoteInternal<TYPES>),
    /// Commit vote
    Commit(ViewSyncVoteInternal<TYPES>),
    /// Finalize vote
    Finalize(ViewSyncVoteInternal<TYPES>),
}

impl<TYPES: NodeType> ViewSyncVote<TYPES> {
    /// Get the encoded signature.
    pub fn signature(&self) -> EncodedSignature {
        match &self {
            ViewSyncVote::PreCommit(vote_internal)
            | ViewSyncVote::Commit(vote_internal)
            | ViewSyncVote::Finalize(vote_internal) => vote_internal.signature.1.clone(),
        }
    }
    /// Get the signature key.
    /// # Panics
    /// If the deserialization fails.
    pub fn signature_key(&self) -> TYPES::SignatureKey {
        let encoded = match &self {
            ViewSyncVote::PreCommit(vote_internal)
            | ViewSyncVote::Commit(vote_internal)
            | ViewSyncVote::Finalize(vote_internal) => vote_internal.signature.0.clone(),
        };
        <TYPES::SignatureKey as SignatureKey>::from_bytes(&encoded).unwrap()
    }
    /// Get the relay.
    pub fn relay(&self) -> u64 {
        match &self {
            ViewSyncVote::PreCommit(vote_internal)
            | ViewSyncVote::Commit(vote_internal)
            | ViewSyncVote::Finalize(vote_internal) => vote_internal.relay,
        }
    }
    /// Get the round number.
    pub fn round(&self) -> TYPES::Time {
        match &self {
            ViewSyncVote::PreCommit(vote_internal)
            | ViewSyncVote::Commit(vote_internal)
            | ViewSyncVote::Finalize(vote_internal) => vote_internal.round,
        }
    }
}

/// Votes on validating or commitment proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(bound(deserialize = ""))]
pub enum QuorumVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// Posivite vote.
    Yes(YesOrNoVote<TYPES, LEAF>),
    /// Negative vote.
    No(YesOrNoVote<TYPES, LEAF>),
    /// Timeout vote.
    Timeout(TimeoutVote<TYPES, LEAF>),
}

impl<TYPES: NodeType> VoteType<TYPES, Commitment<TYPES::BlockType>> for DAVote<TYPES> {
    fn get_view(&self) -> TYPES::Time {
        self.current_view
    }
    fn get_key(&self) -> <TYPES as NodeType>::SignatureKey {
        self.signature_key()
    }
    fn get_signature(&self) -> EncodedSignature {
        self.signature.1.clone()
    }
    fn get_data(&self) -> VoteData<Commitment<TYPES::BlockType>> {
        self.vote_data.clone()
    }
    fn get_vote_token(&self) -> <TYPES as NodeType>::VoteTokenType {
        self.vote_token.clone()
    }
}

impl<TYPES: NodeType> DAVote<TYPES> {
    /// Get the signature key.
    /// # Panics
    /// If the deserialization fails.
    pub fn signature_key(&self) -> TYPES::SignatureKey {
        <TYPES::SignatureKey as SignatureKey>::from_bytes(&self.signature.0).unwrap()
    }
}

impl<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> VoteType<TYPES, Commitment<LEAF>>
    for QuorumVote<TYPES, LEAF>
{
    fn get_view(&self) -> TYPES::Time {
        match self {
            QuorumVote::Yes(v) | QuorumVote::No(v) => v.current_view,
            QuorumVote::Timeout(v) => v.current_view,
        }
    }

    fn get_key(&self) -> <TYPES as NodeType>::SignatureKey {
        self.signature_key()
    }
    fn get_signature(&self) -> EncodedSignature {
        self.signature()
    }
    fn get_data(&self) -> VoteData<Commitment<LEAF>> {
        match self {
            QuorumVote::Yes(v) | QuorumVote::No(v) => v.vote_data.clone(),
            QuorumVote::Timeout(_) => unimplemented!(),
        }
    }
    fn get_vote_token(&self) -> <TYPES as NodeType>::VoteTokenType {
        match self {
            QuorumVote::Yes(v) | QuorumVote::No(v) => v.vote_token.clone(),
            QuorumVote::Timeout(_) => unimplemented!(),
        }
    }
}

impl<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> QuorumVote<TYPES, LEAF> {
    /// Get the encoded signature.

    pub fn signature(&self) -> EncodedSignature {
        match &self {
            Self::Yes(vote) | Self::No(vote) => vote.signature.1.clone(),
            Self::Timeout(vote) => vote.signature.1.clone(),
        }
    }
    /// Get the signature key.
    /// # Panics
    /// If the deserialization fails.

    pub fn signature_key(&self) -> TYPES::SignatureKey {
        let encoded = match &self {
            Self::Yes(vote) | Self::No(vote) => vote.signature.0.clone(),
            Self::Timeout(vote) => vote.signature.0.clone(),
        };
        <TYPES::SignatureKey as SignatureKey>::from_bytes(&encoded).unwrap()
    }
}

impl<TYPES: NodeType> VoteType<TYPES, Commitment<ViewSyncData<TYPES>>> for ViewSyncVote<TYPES> {
    fn get_view(&self) -> TYPES::Time {
        match self {
            ViewSyncVote::PreCommit(v) | ViewSyncVote::Commit(v) | ViewSyncVote::Finalize(v) => {
                v.round
            }
        }
    }
    fn get_key(&self) -> <TYPES as NodeType>::SignatureKey {
        self.signature_key()
    }

    fn get_signature(&self) -> EncodedSignature {
        self.signature()
    }
    fn get_data(&self) -> VoteData<Commitment<ViewSyncData<TYPES>>> {
        match self {
            ViewSyncVote::PreCommit(vote_internal)
            | ViewSyncVote::Commit(vote_internal)
            | ViewSyncVote::Finalize(vote_internal) => vote_internal.vote_data.clone(),
        }
    }

    fn get_vote_token(&self) -> <TYPES as NodeType>::VoteTokenType {
        match self {
            ViewSyncVote::PreCommit(vote_internal)
            | ViewSyncVote::Commit(vote_internal)
            | ViewSyncVote::Finalize(vote_internal) => vote_internal.vote_token.clone(),
        }
    }
}

/// The aggreation of votes, implemented by `VoteAccumulator`.
pub trait Accumulator<T, U>: Sized {
    /// Accumate the `val` to the current state.
    ///
    /// If a threshold is reached, returns `U` (e.g., a certificate). Else, returns `Self` and
    /// continues accumulating items.
    fn append(self, val: T) -> Either<Self, U>;
}

/// Accumulator trait used to accumulate votes into an `AssembledSignature`
pub trait Accumulator2<
    TYPES: NodeType,
    COMMITTABLE: Committable + Serialize + Clone,
    VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
>: Sized
{
    /// Append 1 vote to the accumulator.  If the threshold is not reached, return
    /// the accumulator, else return the `AssembledSignature`
    /// Only called from inside `accumulate_internal`
    fn append(
        self,
        vote: VOTE,
        vote_node_id: usize,
        stake_table_entries: Vec<<TYPES::SignatureKey as SignatureKey>::StakeTableEntry>,
    ) -> Either<Self, AssembledSignature<TYPES>>;
}

/// Accumulates DA votes
pub struct DAVoteAccumulator<
    TYPES: NodeType,
    COMMITTABLE: Committable + Serialize + Clone,
    VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
> {
    /// Map of all da signatures accumlated so far
    pub da_vote_outcomes: VoteMap<Commitment<COMMITTABLE>, TYPES::VoteTokenType>,
    /// A quorum's worth of stake, generally 2f + 1
    pub success_threshold: NonZeroU64,
    /// A list of valid signatures for certificate aggregation
    pub sig_lists: Vec<<TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType>,
    /// A bitvec to indicate which node is active and send out a valid signature for certificate aggregation, this automatically do uniqueness check
    pub signers: BitVec,
    /// Phantom data to specify the vote this accumulator is for
    pub phantom: PhantomData<VOTE>,
}

impl<
        TYPES: NodeType,
        COMMITTABLE: Committable + Serialize + Clone,
        VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
    > Accumulator2<TYPES, COMMITTABLE, VOTE> for DAVoteAccumulator<TYPES, COMMITTABLE, VOTE>
{
    fn append(
        mut self,
        vote: VOTE,
        vote_node_id: usize,
        stake_table_entries: Vec<<TYPES::SignatureKey as SignatureKey>::StakeTableEntry>,
    ) -> Either<Self, AssembledSignature<TYPES>> {
        let VoteData::DA(vote_commitment) = vote.get_data() else {
            return Either::Left(self);
        };

        let encoded_key = vote.get_key().to_bytes();

        // Deserialize the signature so that it can be assembeld into a QC
        // TODO ED Update this once we've gotten rid of EncodedSignature
        let original_signature: <TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType =
            bincode_opts()
                .deserialize(&vote.get_signature().0)
                .expect("Deserialization on the signature shouldn't be able to fail.");

        let (da_stake_casted, da_vote_map) = self
            .da_vote_outcomes
            .entry(vote_commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        // Check for duplicate vote
        // TODO ED Re-encoding signature key to bytes until we get rid of EncodedKey
        // Have to do this because SignatureKey is not hashable
        if da_vote_map.contains_key(&encoded_key) {
            return Either::Left(self);
        }

        if self.signers.get(vote_node_id).as_deref() == Some(&true) {
            error!("Node id is already in signers list");
            return Either::Left(self);
        }
        self.signers.set(vote_node_id, true);
        self.sig_lists.push(original_signature);

        // Already checked that vote data was for a DA vote above
        *da_stake_casted += u64::from(vote.get_vote_token().vote_count());
        da_vote_map.insert(
            encoded_key,
            (vote.get_signature(), vote.get_data(), vote.get_vote_token()),
        );

        if *da_stake_casted >= u64::from(self.success_threshold) {
            // Assemble QC
            let real_qc_pp = <TYPES::SignatureKey as SignatureKey>::get_public_parameter(
                // TODO ED Something about stake table entries.  Might be easier to just pass in membership?
                stake_table_entries.clone(),
                U256::from(self.success_threshold.get()),
            );

            let real_qc_sig = <TYPES::SignatureKey as SignatureKey>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            );

            self.da_vote_outcomes.remove(&vote_commitment);

            return Either::Right(AssembledSignature::DA(real_qc_sig));
        }
        Either::Left(self)
    }
}

/// Accumulate quorum votes
pub struct QuorumVoteAccumulator<
    TYPES: NodeType,
    COMMITTABLE: Committable + Serialize + Clone,
    VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
> {
    /// Map of all signatures accumlated so far
    pub total_vote_outcomes: VoteMap<Commitment<COMMITTABLE>, TYPES::VoteTokenType>,
    /// Map of all yes signatures accumlated so far
    pub yes_vote_outcomes: VoteMap<Commitment<COMMITTABLE>, TYPES::VoteTokenType>,
    /// Map of all no signatures accumlated so far
    pub no_vote_outcomes: VoteMap<Commitment<COMMITTABLE>, TYPES::VoteTokenType>,

    /// A quorum's worth of stake, generally 2f + 1
    pub success_threshold: NonZeroU64,
    /// A failure threshold, generally f + 1
    pub failure_threshold: NonZeroU64,
    /// A list of valid signatures for certificate aggregation
    pub sig_lists: Vec<<TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType>,
    /// A bitvec to indicate which node is active and send out a valid signature for certificate aggregation, this automatically do uniqueness check
    pub signers: BitVec,
    /// Phantom data to ensure this struct is over a specific `VoteType` implementation
    pub phantom: PhantomData<VOTE>,
}

impl<
        TYPES: NodeType,
        COMMITTABLE: Committable + Serialize + Clone,
        VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
    > Accumulator2<TYPES, COMMITTABLE, VOTE> for QuorumVoteAccumulator<TYPES, COMMITTABLE, VOTE>
{
    fn append(
        mut self,
        vote: VOTE,
        vote_node_id: usize,
        stake_table_entries: Vec<<TYPES::SignatureKey as SignatureKey>::StakeTableEntry>,
    ) -> Either<Self, AssembledSignature<TYPES>> {
        let (VoteData::Yes(vote_commitment) | VoteData::No(vote_commitment)) = vote.get_data()
        else {
            return Either::Left(self);
        };

        let encoded_key = vote.get_key().to_bytes();

        // Deserialize the signature so that it can be assembeld into a QC
        // TODO ED Update this once we've gotten rid of EncodedSignature
        let original_signature: <TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType =
            bincode_opts()
                .deserialize(&vote.get_signature().0)
                .expect("Deserialization on the signature shouldn't be able to fail.");

        let (total_stake_casted, total_vote_map) = self
            .total_vote_outcomes
            .entry(vote_commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (yes_stake_casted, yes_vote_map) = self
            .yes_vote_outcomes
            .entry(vote_commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (no_stake_casted, no_vote_map) = self
            .no_vote_outcomes
            .entry(vote_commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        // Check for duplicate vote
        // TODO ED Re-encoding signature key to bytes until we get rid of EncodedKey
        // Have to do this because SignatureKey is not hashable
        if total_vote_map.contains_key(&encoded_key) {
            return Either::Left(self);
        }

        if self.signers.get(vote_node_id).as_deref() == Some(&true) {
            error!("Node id is already in signers list");
            return Either::Left(self);
        }
        self.signers.set(vote_node_id, true);
        self.sig_lists.push(original_signature);

        // TODO ED Make all these get calls as local variables to avoid constantly calling them
        *total_stake_casted += u64::from(vote.get_vote_token().vote_count());
        total_vote_map.insert(
            encoded_key.clone(),
            (vote.get_signature(), vote.get_data(), vote.get_vote_token()),
        );

        match vote.get_data() {
            VoteData::Yes(_) => {
                *yes_stake_casted += u64::from(vote.get_vote_token().vote_count());
                yes_vote_map.insert(
                    encoded_key,
                    (vote.get_signature(), vote.get_data(), vote.get_vote_token()),
                );
            }
            VoteData::No(_) => {
                *no_stake_casted += u64::from(vote.get_vote_token().vote_count());
                no_vote_map.insert(
                    encoded_key,
                    (vote.get_signature(), vote.get_data(), vote.get_vote_token()),
                );
            }
            _ => return Either::Left(self),
        }

        if *total_stake_casted >= u64::from(self.success_threshold) {
            // Assemble QC
            let real_qc_pp = <TYPES::SignatureKey as SignatureKey>::get_public_parameter(
                // TODO ED Something about stake table entries.  Might be easier to just pass in membership?
                stake_table_entries.clone(),
                U256::from(self.success_threshold.get()),
            );

            let real_qc_sig = <TYPES::SignatureKey as SignatureKey>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            );

            if *yes_stake_casted >= u64::from(self.success_threshold) {
                self.yes_vote_outcomes.remove(&vote_commitment);
                return Either::Right(AssembledSignature::Yes(real_qc_sig));
            } else if *no_stake_casted >= u64::from(self.failure_threshold) {
                self.total_vote_outcomes.remove(&vote_commitment);
                return Either::Right(AssembledSignature::No(real_qc_sig));
            }
        }
        Either::Left(self)
    }
}

/// Accumulates view sync votes
pub struct ViewSyncVoteAccumulator<
    TYPES: NodeType,
    COMMITTABLE: Committable + Serialize + Clone,
    VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
> {
    /// Map of all pre_commit signatures accumlated so far
    pub pre_commit_vote_outcomes: VoteMap<Commitment<COMMITTABLE>, TYPES::VoteTokenType>,
    /// Map of all ommit signatures accumlated so far
    pub commit_vote_outcomes: VoteMap<Commitment<COMMITTABLE>, TYPES::VoteTokenType>,
    /// Map of all finalize signatures accumlated so far
    pub finalize_vote_outcomes: VoteMap<Commitment<COMMITTABLE>, TYPES::VoteTokenType>,

    /// A quorum's worth of stake, generally 2f + 1
    pub success_threshold: NonZeroU64,
    /// A quorum's failure threshold, generally f + 1
    pub failure_threshold: NonZeroU64,
    /// A list of valid signatures for certificate aggregation
    pub sig_lists: Vec<<TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType>,
    /// A bitvec to indicate which node is active and send out a valid signature for certificate aggregation, this automatically do uniqueness check
    pub signers: BitVec,
    /// Phantom data since we want the accumulator to be attached to a single `VoteType`  
    pub phantom: PhantomData<VOTE>,
}

impl<
        TYPES: NodeType,
        COMMITTABLE: Committable + Serialize + Clone,
        VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
    > Accumulator2<TYPES, COMMITTABLE, VOTE> for ViewSyncVoteAccumulator<TYPES, COMMITTABLE, VOTE>
{
    #[allow(clippy::too_many_lines)]
    fn append(
        mut self,
        vote: VOTE,
        vote_node_id: usize,
        stake_table_entries: Vec<<TYPES::SignatureKey as SignatureKey>::StakeTableEntry>,
    ) -> Either<Self, AssembledSignature<TYPES>> {
        let (VoteData::ViewSyncPreCommit(vote_commitment)
        | VoteData::ViewSyncCommit(vote_commitment)
        | VoteData::ViewSyncFinalize(vote_commitment)) = vote.get_data()
        else {
            return Either::Left(self);
        };

        // error!("Vote is {:?}", vote.clone());

        let encoded_key = vote.get_key().to_bytes();

        // Deserialize the signature so that it can be assembeld into a QC
        // TODO ED Update this once we've gotten rid of EncodedSignature
        let original_signature: <TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType =
            bincode_opts()
                .deserialize(&vote.get_signature().0)
                .expect("Deserialization on the signature shouldn't be able to fail.");

        let (pre_commit_stake_casted, pre_commit_vote_map) = self
            .pre_commit_vote_outcomes
            .entry(vote_commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        // Check for duplicate vote
        if pre_commit_vote_map.contains_key(&encoded_key) {
            return Either::Left(self);
        }

        let (commit_stake_casted, commit_vote_map) = self
            .commit_vote_outcomes
            .entry(vote_commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        if commit_vote_map.contains_key(&encoded_key) {
            return Either::Left(self);
        }

        let (finalize_stake_casted, finalize_vote_map) = self
            .finalize_vote_outcomes
            .entry(vote_commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        if finalize_vote_map.contains_key(&encoded_key) {
            return Either::Left(self);
        }

        // update the active_keys and sig_lists
        // TODO ED Possible bug where a node sends precommit vote and then commit vote after
        // precommit cert is formed, their commit vote won't be counted because of this check
        // Probably need separate signers vecs.
        if self.signers.get(vote_node_id).as_deref() == Some(&true) {
            error!("node id already in signers");
            return Either::Left(self);
        }
        self.signers.set(vote_node_id, true);
        self.sig_lists.push(original_signature);

        match vote.get_data() {
            VoteData::ViewSyncPreCommit(_) => {
                *pre_commit_stake_casted += u64::from(vote.get_vote_token().vote_count());
                pre_commit_vote_map.insert(
                    encoded_key,
                    (vote.get_signature(), vote.get_data(), vote.get_vote_token()),
                );
            }
            VoteData::ViewSyncCommit(_) => {
                *commit_stake_casted += u64::from(vote.get_vote_token().vote_count());
                commit_vote_map.insert(
                    encoded_key,
                    (vote.get_signature(), vote.get_data(), vote.get_vote_token()),
                );
            }
            VoteData::ViewSyncFinalize(_) => {
                *finalize_stake_casted += u64::from(vote.get_vote_token().vote_count());
                finalize_vote_map.insert(
                    encoded_key,
                    (vote.get_signature(), vote.get_data(), vote.get_vote_token()),
                );
            }
            _ => unimplemented!(),
        }

        if *pre_commit_stake_casted >= u64::from(self.failure_threshold) {
            let real_qc_pp = <TYPES::SignatureKey as SignatureKey>::get_public_parameter(
                stake_table_entries,
                U256::from(self.failure_threshold.get()),
            );

            let real_qc_sig = <TYPES::SignatureKey as SignatureKey>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            );

            self.pre_commit_vote_outcomes
                .remove(&vote_commitment)
                .unwrap();
            return Either::Right(AssembledSignature::ViewSyncPreCommit(real_qc_sig));
        }

        if *commit_stake_casted >= u64::from(self.success_threshold) {
            let real_qc_pp = <TYPES::SignatureKey as SignatureKey>::get_public_parameter(
                stake_table_entries.clone(),
                U256::from(self.success_threshold.get()),
            );

            let real_qc_sig = <TYPES::SignatureKey as SignatureKey>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            );
            self.commit_vote_outcomes.remove(&vote_commitment).unwrap();
            return Either::Right(AssembledSignature::ViewSyncCommit(real_qc_sig));
        }

        if *finalize_stake_casted >= u64::from(self.success_threshold) {
            let real_qc_pp = <TYPES::SignatureKey as SignatureKey>::get_public_parameter(
                stake_table_entries.clone(),
                U256::from(self.success_threshold.get()),
            );

            let real_qc_sig = <TYPES::SignatureKey as SignatureKey>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            );
            self.finalize_vote_outcomes
                .remove(&vote_commitment)
                .unwrap();
            return Either::Right(AssembledSignature::ViewSyncFinalize(real_qc_sig));
        }

        Either::Left(self)
    }
}

/// Placeholder accumulator; will be replaced by accumulator for each certificate type
pub struct AccumulatorPlaceholder<
    TYPES: NodeType,
    COMMITTABLE: Committable + Serialize + Clone,
    VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
> {
    /// Phantom data to make compiler happy
    pub phantom: PhantomData<(TYPES, VOTE, COMMITTABLE)>,
}

impl<
        TYPES: NodeType,
        COMMITTABLE: Committable + Serialize + Clone,
        VOTE: VoteType<TYPES, Commitment<COMMITTABLE>>,
    > Accumulator2<TYPES, COMMITTABLE, VOTE> for AccumulatorPlaceholder<TYPES, COMMITTABLE, VOTE>
{
    fn append(
        self,
        _vote: VOTE,
        _vote_node_id: usize,
        _stake_table_entries: Vec<<TYPES::SignatureKey as SignatureKey>::StakeTableEntry>,
    ) -> Either<Self, AssembledSignature<TYPES>> {
        either::Left(self)
    }
}

/// Mapping of commitments to vote tokens by key.
// TODO ED Remove this whole token generic
type VoteMap<COMMITMENT, TOKEN> = HashMap<
    COMMITMENT,
    (
        u64,
        BTreeMap<EncodedPublicKey, (EncodedSignature, VoteData<COMMITMENT>, TOKEN)>,
    ),
>;

/// Describe the process of collecting signatures on block or leaf commitment, to form a DAC or QC,
/// respectively.
///
/// TODO GG used only in election.rs; move this to there and make it private?
pub struct VoteAccumulator<
    TOKEN,
    COMMITMENT: Serialize + for<'a> Deserialize<'a> + Clone,
    TYPES: NodeType,
> {
    /// Map of all signatures accumlated so far
    pub total_vote_outcomes: VoteMap<COMMITMENT, TOKEN>,
    /// Map of all da signatures accumlated so far
    pub da_vote_outcomes: VoteMap<COMMITMENT, TOKEN>,
    /// Map of all yes signatures accumlated so far
    pub yes_vote_outcomes: VoteMap<COMMITMENT, TOKEN>,
    /// Map of all no signatures accumlated so far
    pub no_vote_outcomes: VoteMap<COMMITMENT, TOKEN>,
    /// Map of all view sync precommit votes accumulated thus far
    pub viewsync_precommit_vote_outcomes: VoteMap<COMMITMENT, TOKEN>,
    /// Map of all view sync commit votes accumulated thus far
    pub viewsync_commit_vote_outcomes: VoteMap<COMMITMENT, TOKEN>,
    /// Map of all view sync finalize votes accumulated thus far
    pub viewsync_finalize_vote_outcomes: VoteMap<COMMITMENT, TOKEN>,
    /// A quorum's worth of stake, generall 2f + 1
    pub success_threshold: NonZeroU64,
    /// Enough stake to know that we cannot possibly get a quorum, generally f + 1
    pub failure_threshold: NonZeroU64,
    /// A list of valid signatures for certificate aggregation
    pub sig_lists: Vec<<TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType>,
    /// A bitvec to indicate which node is active and send out a valid signature for certificate aggregation, this automatically do uniqueness check
    pub signers: BitVec,
}

impl<TOKEN, LEAF: Committable + Serialize + Clone, TYPES: NodeType>
    Accumulator<
        (
            Commitment<LEAF>,
            (
                EncodedPublicKey,
                (
                    EncodedSignature,
                    Vec<<TYPES::SignatureKey as SignatureKey>::StakeTableEntry>,
                    usize,
                    VoteData<Commitment<LEAF>>,
                    TOKEN,
                ),
            ),
        ),
        AssembledSignature<TYPES>,
    > for VoteAccumulator<TOKEN, Commitment<LEAF>, TYPES>
where
    TOKEN: Clone + VoteToken,
{
    #![allow(clippy::too_many_lines)]
    fn append(
        mut self,
        val: (
            Commitment<LEAF>,
            (
                EncodedPublicKey,
                (
                    EncodedSignature,
                    Vec<<TYPES::SignatureKey as SignatureKey>::StakeTableEntry>,
                    usize,
                    VoteData<Commitment<LEAF>>,
                    TOKEN,
                ),
            ),
        ),
    ) -> Either<Self, AssembledSignature<TYPES>> {
        let (commitment, (key, (sig, entries, node_id, vote_data, token))) = val;

        // Desereialize the sig so that it can be assembeld into a QC
        let original_signature: <TYPES::SignatureKey as SignatureKey>::PureAssembledSignatureType =
            bincode_opts()
                .deserialize(&sig.0)
                .expect("Deserialization on the signature shouldn't be able to fail.");

        let (total_stake_casted, total_vote_map) = self
            .total_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        // Check for duplicate vote
        if total_vote_map.contains_key(&key) {
            return Either::Left(self);
        }
        let (da_stake_casted, da_vote_map) = self
            .da_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (yes_stake_casted, yes_vote_map) = self
            .yes_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (no_stake_casted, no_vote_map) = self
            .no_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (viewsync_precommit_stake_casted, viewsync_precommit_vote_map) = self
            .viewsync_precommit_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (viewsync_commit_stake_casted, viewsync_commit_vote_map) = self
            .viewsync_commit_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (viewsync_finalize_stake_casted, viewsync_finalize_vote_map) = self
            .viewsync_finalize_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        // Accumulate the stake for each leaf commitment rather than the total
        // stake of all votes, in case they correspond to inconsistent
        // commitments.

        // update the active_keys and sig_lists
        if self.signers.get(node_id).as_deref() == Some(&true) {
            error!("node id already in signers");
            return Either::Left(self);
        }
        self.signers.set(node_id, true);
        self.sig_lists.push(original_signature);

        *total_stake_casted += u64::from(token.vote_count());
        total_vote_map.insert(key.clone(), (sig.clone(), vote_data.clone(), token.clone()));

        match vote_data {
            VoteData::DA(_) => {
                *da_stake_casted += u64::from(token.vote_count());
                da_vote_map.insert(key, (sig, vote_data, token));
            }
            VoteData::Yes(_) => {
                *yes_stake_casted += u64::from(token.vote_count());
                yes_vote_map.insert(key, (sig, vote_data, token));
            }
            VoteData::No(_) => {
                *no_stake_casted += u64::from(token.vote_count());
                no_vote_map.insert(key, (sig, vote_data, token));
            }
            VoteData::ViewSyncPreCommit(_) => {
                *viewsync_precommit_stake_casted += u64::from(token.vote_count());
                viewsync_precommit_vote_map.insert(key, (sig, vote_data, token));
            }
            VoteData::ViewSyncCommit(_) => {
                *viewsync_commit_stake_casted += u64::from(token.vote_count());
                viewsync_commit_vote_map.insert(key, (sig, vote_data, token));
            }
            VoteData::ViewSyncFinalize(_) => {
                *viewsync_finalize_stake_casted += u64::from(token.vote_count());
                viewsync_finalize_vote_map.insert(key, (sig, vote_data, token));
            }
            VoteData::Timeout(_) => {
                unimplemented!()
            }
        }

        // This is a messy way of accounting for the different vote types, but we will be replacing this code very soon
        if *total_stake_casted >= u64::from(self.success_threshold) {
            // Do assemble for QC here
            let real_qc_pp = <TYPES::SignatureKey as SignatureKey>::get_public_parameter(
                entries.clone(),
                U256::from(self.success_threshold.get()),
            );

            let real_qc_sig = <TYPES::SignatureKey as SignatureKey>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            );

            if *yes_stake_casted >= u64::from(self.success_threshold) {
                self.yes_vote_outcomes.remove(&commitment);
                return Either::Right(AssembledSignature::Yes(real_qc_sig));
            } else if *no_stake_casted >= u64::from(self.failure_threshold) {
                self.total_vote_outcomes.remove(&commitment);
                return Either::Right(AssembledSignature::No(real_qc_sig));
            } else if *da_stake_casted >= u64::from(self.success_threshold) {
                self.da_vote_outcomes.remove(&commitment);
                return Either::Right(AssembledSignature::DA(real_qc_sig));
            } else if *viewsync_commit_stake_casted >= u64::from(self.success_threshold) {
                self.viewsync_commit_vote_outcomes
                    .remove(&commitment)
                    .unwrap();
                return Either::Right(AssembledSignature::ViewSyncCommit(real_qc_sig));
            } else if *viewsync_finalize_stake_casted >= u64::from(self.success_threshold) {
                self.viewsync_finalize_vote_outcomes
                    .remove(&commitment)
                    .unwrap();
                return Either::Right(AssembledSignature::ViewSyncFinalize(real_qc_sig));
            }
        }
        if *viewsync_precommit_stake_casted >= u64::from(self.failure_threshold) {
            let real_qc_pp = <TYPES::SignatureKey as SignatureKey>::get_public_parameter(
                entries,
                U256::from(self.failure_threshold.get()),
            );

            let real_qc_sig = <TYPES::SignatureKey as SignatureKey>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            );

            self.viewsync_precommit_vote_outcomes
                .remove(&commitment)
                .unwrap();
            return Either::Right(AssembledSignature::ViewSyncPreCommit(real_qc_sig));
        }
        Either::Left(self)
    }
}
