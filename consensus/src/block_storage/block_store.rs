// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    block_storage::{
        block_tree::BlockTree,
        tracing::{observe_block, BlockStage},
        BlockReader,
    },
    counters,
    logging::{LogEvent, LogSchema},
    persistent_liveness_storage::{
        PersistentLivenessStorage, RecoveryData, RootInfo, RootMetadata,
    },
    state_replication::StateComputer,
    util::time_service::TimeService,
};
use anyhow::{bail, ensure, format_err, Context};
use consensus_types::{
    block::Block, executed_block::ExecutedBlock, quorum_cert::QuorumCert, sync_info::SyncInfo,
    timeout_certificate::TimeoutCertificate,
};
use diem_crypto::HashValue;
use diem_infallible::RwLock;
use diem_logger::prelude::*;
use diem_types::{ledger_info::LedgerInfoWithSignatures, transaction::TransactionStatus};
use executor_types::{Error, StateComputeResult};
use short_hex_str::AsShortHexStr;
use std::{collections::vec_deque::VecDeque, sync::Arc, time::Duration};

#[cfg(test)]
#[path = "block_store_test.rs"]
mod block_store_test;

#[cfg(test)]
#[path = "block_store_and_lec_recovery_test.rs"]
mod block_store_and_lec_recovery_test;

#[path = "sync_manager.rs"]
pub mod sync_manager;

fn update_counters_for_ordered_blocks(ordered_blocks: &[Arc<ExecutedBlock>]) {
    for block in ordered_blocks {
        observe_block(block.block().timestamp_usecs(), BlockStage::ORDERED);
    }
}

fn update_counters_for_committed_blocks(blocks_to_commit: &[Arc<ExecutedBlock>]) {
    for block in blocks_to_commit {
        observe_block(block.block().timestamp_usecs(), BlockStage::COMMITTED);
        let txn_status = block.compute_result().compute_status();
        counters::NUM_TXNS_PER_BLOCK.observe(txn_status.len() as f64);
        counters::COMMITTED_BLOCKS_COUNT.inc();
        counters::LAST_COMMITTED_ROUND.set(block.round() as i64);
        counters::LAST_COMMITTED_VERSION.set(block.compute_result().num_leaves() as i64);

        for status in txn_status.iter() {
            match status {
                TransactionStatus::Keep(_) => {
                    counters::COMMITTED_TXNS_COUNT
                        .with_label_values(&["success"])
                        .inc();
                }
                TransactionStatus::Discard(_) => {
                    counters::COMMITTED_TXNS_COUNT
                        .with_label_values(&["failed"])
                        .inc();
                }
                TransactionStatus::Retry => {
                    counters::COMMITTED_TXNS_COUNT
                        .with_label_values(&["retry"])
                        .inc();
                }
            }
        }
    }
}

/// Responsible for maintaining all the blocks of payload and the dependencies of those blocks
/// (parent and previous QC links).  It is expected to be accessed concurrently by multiple threads
/// and is thread-safe.
///
/// Example tree block structure based on parent links.
///                         ╭--> A3
/// Genesis--> B0--> B1--> B2--> B3
///             ╰--> C1--> C2
///                         ╰--> D3
///
/// Example corresponding tree block structure for the QC links (must follow QC constraints).
///                         ╭--> A3
/// Genesis--> B0--> B1--> B2--> B3
///             ├--> C1
///             ├--------> C2
///             ╰--------------> D3
pub struct BlockStore {
    inner: Arc<RwLock<BlockTree>>,
    state_computer: Arc<dyn StateComputer>,
    /// The persistent storage backing up the in-memory data structure, every write should go
    /// through this before in-memory tree.
    storage: Arc<dyn PersistentLivenessStorage>,
    /// Used to ensure that any block stored will have a timestamp < the local time
    time_service: Arc<dyn TimeService>,
}

pub fn update_counters_and_prune_blocks(
    block_tree: Arc<RwLock<BlockTree>>,
    storage: Arc<dyn PersistentLivenessStorage>,
    root: Arc<ExecutedBlock>,
    blocks_to_commit: &[Arc<ExecutedBlock>],
) {
    let block_to_commit = blocks_to_commit.last().unwrap().clone();
    update_counters_for_committed_blocks(blocks_to_commit);
    let current_round = root.round();
    let committed_round = block_to_commit.round();
    debug!(
        LogSchema::new(LogEvent::CommitViaBlock).round(current_round),
        committed_round = committed_round,
        block_id = block_to_commit.id(),
    );
    event!("committed",
        "block_id": block_to_commit.id().short_str(),
        "epoch": block_to_commit.epoch(),
        "round": committed_round,
        "parent_id": block_to_commit.parent_id().short_str(),
    );

    let id_to_remove = block_tree.read().find_blocks_to_prune(block_to_commit.id());
    if let Err(e) = storage.prune_tree(id_to_remove.clone().into_iter().collect()) {
        // it's fine to fail here, as long as the commit succeeds, the next restart will clean
        // up dangling blocks, and we need to prune the tree to keep the root consistent with
        // executor.
        error!(error = ?e, "fail to delete block");
    }
    block_tree
        .write()
        .update_commit_id_and_process_pruned_blocks(block_to_commit.id(), id_to_remove);
}

impl BlockStore {
    pub fn new(
        storage: Arc<dyn PersistentLivenessStorage>,
        initial_data: RecoveryData,
        state_computer: Arc<dyn StateComputer>,
        max_pruned_blocks_in_mem: usize,
        time_service: Arc<dyn TimeService>,
    ) -> Self {
        let highest_tc = initial_data.highest_timeout_certificate();
        let (root, root_metadata, blocks, quorum_certs) = initial_data.take();
        Self::build(
            root,
            root_metadata,
            blocks,
            quorum_certs,
            highest_tc,
            state_computer,
            storage,
            max_pruned_blocks_in_mem,
            time_service,
        )
    }

    fn build(
        root: RootInfo,
        root_metadata: RootMetadata,
        blocks: Vec<Block>,
        quorum_certs: Vec<QuorumCert>,
        highest_timeout_cert: Option<TimeoutCertificate>,
        state_computer: Arc<dyn StateComputer>,
        storage: Arc<dyn PersistentLivenessStorage>,
        max_pruned_blocks_in_mem: usize,
        time_service: Arc<dyn TimeService>,
    ) -> Self {
        let RootInfo(root_block, root_qc, root_li) = root;
        //verify root is correct
        assert_eq!(
            root_qc.certified_block().version(),
            root_metadata.version(),
            "root qc version {} doesn't match committed trees {}",
            root_qc.certified_block().version(),
            root_metadata.version(),
        );
        assert_eq!(
            root_qc.certified_block().executed_state_id(),
            root_metadata.accu_hash,
            "root qc state id {} doesn't match committed trees {}",
            root_qc.certified_block().executed_state_id(),
            root_metadata.accu_hash,
        );

        let result = StateComputeResult::new(
            root_metadata.accu_hash,
            root_metadata.frozen_root_hashes,
            root_metadata.num_leaves, /* num_leaves */
            vec![],                   /* parent_root_hashes */
            0,                        /* parent_num_leaves */
            None,                     /* epoch_state */
            vec![],                   /* compute_status */
            vec![],                   /* txn_infos */
            vec![],                   /* reconfig_events */
        );

        let executed_root_block = ExecutedBlock::new(
            root_block,
            // Create a dummy state_compute_result with necessary fields filled in.
            result,
        );

        let tree = BlockTree::new(
            executed_root_block,
            root_qc,
            root_li,
            max_pruned_blocks_in_mem,
            highest_timeout_cert.map(Arc::new),
        );
        let block_store = Self {
            inner: Arc::new(RwLock::new(tree)),
            state_computer,
            storage,
            time_service,
        };
        for block in blocks {
            block_store
                .execute_and_insert_block(block)
                .unwrap_or_else(|e| {
                    panic!("[BlockStore] failed to insert block during build {:?}", e)
                });
        }
        for qc in quorum_certs {
            block_store
                .insert_single_quorum_cert(qc)
                .unwrap_or_else(|e| {
                    panic!("[BlockStore] failed to insert quorum during build{:?}", e)
                });
        }
        counters::LAST_COMMITTED_ROUND.set(block_store.root().round() as i64);
        counters::LAST_COMMITTED_VERSION
            .set(block_store.root().compute_result().num_leaves() as i64);
        block_store
    }

    /// Commit the given block id with the proof, returns () on success or error
    pub async fn commit(&self, finality_proof: LedgerInfoWithSignatures) -> anyhow::Result<()> {
        let block_id_to_commit = finality_proof.ledger_info().consensus_block_id();
        let block_to_commit = self
            .get_block(block_id_to_commit)
            .ok_or_else(|| format_err!("Committed block id not found"))?;

        // First make sure that this commit is new.
        ensure!(
            block_to_commit.round() > self.root().round(),
            "Committed block round lower than root"
        );

        let blocks_to_commit = self.path_from_root(block_id_to_commit).unwrap_or_default();

        let block_tree = self.inner.clone();
        let storage = self.storage.clone();
        let root = self.root();

        self.inner.write().update_root_id(block_to_commit.id());
        update_counters_for_ordered_blocks(&blocks_to_commit);

        // asynchronously execute and commit
        self.state_computer
            .commit(
                &blocks_to_commit,
                finality_proof,
                Box::new(move |executed_blocks: &[Arc<ExecutedBlock>]| {
                    update_counters_and_prune_blocks(block_tree, storage, root, executed_blocks);
                }),
            )
            .await
            .expect("Failed to persist commit");

        Ok(())
    }

    pub async fn rebuild(
        &self,
        root: RootInfo,
        root_metadata: RootMetadata,
        blocks: Vec<Block>,
        quorum_certs: Vec<QuorumCert>,
    ) {
        let max_pruned_blocks_in_mem = self.inner.read().max_pruned_blocks_in_mem();
        // Rollover the previous highest TC from the old tree to the new one.
        let prev_htc = self.highest_timeout_cert().map(|tc| tc.as_ref().clone());
        let BlockStore { inner, .. } = Self::build(
            root,
            root_metadata,
            blocks,
            quorum_certs,
            prev_htc,
            Arc::clone(&self.state_computer),
            Arc::clone(&self.storage),
            max_pruned_blocks_in_mem,
            Arc::clone(&self.time_service),
        );
        let to_remove = self.inner.read().get_all_block_id();
        if let Err(e) = self.storage.prune_tree(to_remove) {
            // it's fine to fail here, the next restart will try to clean up dangling blocks again.
            error!(error = ?e, "Fail to delete block from consensus db");
        }
        // Unwrap the new tree and replace the existing tree.
        *self.inner.write() = Arc::try_unwrap(inner)
            .unwrap_or_else(|_| panic!("New block tree is not shared"))
            .into_inner();
        // If we fail to commit B_i via state computer and crash, after restart our highest commit cert
        // will not match the latest commit B_j(j<i) of state computer.
        // This introduces an inconsistent state if we send out SyncInfo and others try to sync to
        // B_i and figure out we only have B_j.
        // Here we commit up to the highest_commit_cert to maintain highest_commit_cert == state_computer.committed_trees.
        if self.highest_commit_cert().commit_info().round() > self.root().round() {
            let finality_proof = self.highest_commit_cert().ledger_info().clone();
            if let Err(e) = self.commit(finality_proof).await {
                error!(error = ?e, "Commit error during rebuild");
            }
        }
    }

    /// Execute and insert a block if it passes all validation tests.
    /// Returns the Arc to the block kept in the block store after persisting it to storage
    ///
    /// This function assumes that the ancestors are present (returns MissingParent otherwise).
    ///
    /// Duplicate inserts will return the previously inserted block (
    /// note that it is considered a valid non-error case, for example, it can happen if a validator
    /// receives a certificate for a block that is currently being added).
    pub fn execute_and_insert_block(&self, block: Block) -> anyhow::Result<Arc<ExecutedBlock>> {
        if let Some(existing_block) = self.get_block(block.id()) {
            return Ok(existing_block);
        }
        ensure!(
            self.inner.read().root().round() < block.round(),
            "Block with old round"
        );

        let executed_block = match self.execute_block(block.clone()) {
            Ok(res) => Ok(res),
            Err(Error::BlockNotFound(parent_block_id)) => {
                // recover the block tree in executor
                let blocks_to_reexecute = self.path_from_root(parent_block_id).unwrap_or_default();

                for block in blocks_to_reexecute {
                    self.execute_block(block.block().clone())?;
                }
                self.execute_block(block)
            }
            err => err,
        }?;

        // ensure local time past the block time
        let block_time = Duration::from_micros(executed_block.timestamp_usecs());
        self.time_service.wait_until(block_time);
        self.storage
            .save_tree(vec![executed_block.block().clone()], vec![])
            .context("Insert block failed when saving block")?;
        self.inner.write().insert_block(executed_block)
    }

    fn execute_block(&self, block: Block) -> anyhow::Result<ExecutedBlock, Error> {
        // Although NIL blocks don't have a payload, we still send a T::default() to compute
        // because we may inject a block prologue transaction.
        let state_compute_result = self.state_computer.compute(&block, block.parent_id())?;
        observe_block(block.timestamp_usecs(), BlockStage::EXECUTED);

        Ok(ExecutedBlock::new(block, state_compute_result))
    }

    /// Validates quorum certificates and inserts it into block tree assuming dependencies exist.
    pub fn insert_single_quorum_cert(&self, qc: QuorumCert) -> anyhow::Result<()> {
        // If the parent block is not the root block (i.e not None), ensure the executed state
        // of a block is consistent with its QuorumCert, otherwise persist the QuorumCert's
        // state and on restart, a new execution will agree with it.  A new execution will match
        // the QuorumCert's state on the next restart will work if there is a memory
        // corruption, for example.
        match self.get_block(qc.certified_block().id()) {
            Some(executed_block) => {
                ensure!(
                    executed_block.block_info() == *qc.certified_block(),
                    "QC for block {} has different {:?} than local {:?}",
                    qc.certified_block().id(),
                    qc.certified_block(),
                    executed_block.block_info()
                );
                observe_block(
                    executed_block.block().timestamp_usecs(),
                    BlockStage::QC_ADDED,
                );
            }
            None => bail!("Insert {} without having the block in store first", qc),
        };

        self.storage
            .save_tree(vec![], vec![qc.clone()])
            .context("Insert block failed when saving quorum")?;
        self.inner.write().insert_quorum_cert(qc)
    }

    /// Replace the highest timeout certificate in case the given one has a higher round.
    /// In case a timeout certificate is updated, persist it to storage.
    pub fn insert_timeout_certificate(&self, tc: Arc<TimeoutCertificate>) -> anyhow::Result<()> {
        let cur_tc_round = self.highest_timeout_cert().map_or(0, |tc| tc.round());
        if tc.round() <= cur_tc_round {
            return Ok(());
        }
        self.storage
            .save_highest_timeout_cert(tc.as_ref().clone())
            .context("Timeout certificate insert failed when persisting to DB")?;
        self.inner.write().replace_timeout_cert(tc);
        Ok(())
    }

    /// Prune the tree up to next_root_id (keep next_root_id's block).  Any branches not part of
    /// the next_root_id's tree should be removed as well.
    ///
    /// For example, root = B0
    /// B0--> B1--> B2
    ///        ╰--> B3--> B4
    ///
    /// prune_tree(B3) should be left with
    /// B3--> B4, root = B3
    ///
    /// Returns the block ids of the blocks removed.
    fn prune_tree(&self, next_root_id: HashValue) -> VecDeque<HashValue> {
        let id_to_remove = self.inner.read().find_blocks_to_prune(next_root_id);
        if let Err(e) = self
            .storage
            .prune_tree(id_to_remove.clone().into_iter().collect())
        {
            // it's fine to fail here, as long as the commit succeeds, the next restart will clean
            // up dangling blocks, and we need to prune the tree to keep the root consistent with
            // executor.
            error!(error = ?e, "fail to delete block");
        }

        // synchronously update both root_id and commit_root_id
        self.inner
            .write()
            .update_root_id(next_root_id)
            .update_commit_id_and_process_pruned_blocks(next_root_id, id_to_remove.clone());
        id_to_remove
    }
}

impl BlockReader for BlockStore {
    fn block_exists(&self, block_id: HashValue) -> bool {
        self.inner.read().block_exists(&block_id)
    }

    fn get_block(&self, block_id: HashValue) -> Option<Arc<ExecutedBlock>> {
        self.inner.read().get_block(&block_id)
    }

    fn root(&self) -> Arc<ExecutedBlock> {
        self.inner.read().root()
    }

    fn get_quorum_cert_for_block(&self, block_id: HashValue) -> Option<Arc<QuorumCert>> {
        self.inner.read().get_quorum_cert_for_block(&block_id)
    }

    fn path_from_root(&self, block_id: HashValue) -> Option<Vec<Arc<ExecutedBlock>>> {
        self.inner.read().path_from_root(block_id)
    }

    fn highest_certified_block(&self) -> Arc<ExecutedBlock> {
        self.inner.read().highest_certified_block()
    }

    fn highest_quorum_cert(&self) -> Arc<QuorumCert> {
        self.inner.read().highest_quorum_cert()
    }

    fn highest_commit_cert(&self) -> Arc<QuorumCert> {
        self.inner.read().highest_commit_cert()
    }

    fn highest_timeout_cert(&self) -> Option<Arc<TimeoutCertificate>> {
        self.inner.read().highest_timeout_cert()
    }

    fn sync_info(&self) -> SyncInfo {
        SyncInfo::new(
            self.highest_quorum_cert().as_ref().clone(),
            self.highest_commit_cert().as_ref().clone(),
            self.highest_timeout_cert().map(|tc| tc.as_ref().clone()),
        )
    }
}

#[cfg(any(test, feature = "fuzzing"))]
impl BlockStore {
    /// Returns the number of blocks in the tree
    pub(crate) fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Returns the number of child links in the tree
    pub(crate) fn child_links(&self) -> usize {
        self.inner.read().child_links()
    }

    /// The number of pruned blocks that are still available in memory
    pub(super) fn pruned_blocks_in_mem(&self) -> usize {
        self.inner.read().pruned_blocks_in_mem()
    }

    /// Helper function to insert the block with the qc together
    pub fn insert_block_with_qc(&self, block: Block) -> anyhow::Result<Arc<ExecutedBlock>> {
        self.insert_single_quorum_cert(block.quorum_cert().clone())?;
        self.execute_and_insert_block(block)
    }
}
