// Built-in
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::{thread, time};
// External
use crate::franklin_crypto::bellman::pairing::ff::PrimeField;
use futures::channel::mpsc;
use log::info;
// Workspace deps
use circuit::witness::change_pubkey_offchain::{
    apply_change_pubkey_offchain_tx, calculate_change_pubkey_offchain_from_witness,
};
use circuit::witness::close_account::apply_close_account_tx;
use circuit::witness::close_account::calculate_close_account_operations_from_witness;
use circuit::witness::deposit::apply_deposit_tx;
use circuit::witness::deposit::calculate_deposit_operations_from_witness;
use circuit::witness::full_exit::{
    apply_full_exit_tx, calculate_full_exit_operations_from_witness,
};
use circuit::witness::transfer::apply_transfer_tx;
use circuit::witness::transfer::calculate_transfer_operations_from_witness;
use circuit::witness::transfer_to_new::apply_transfer_to_new_tx;
use circuit::witness::transfer_to_new::calculate_transfer_to_new_operations_from_witness;
use circuit::witness::utils::prepare_sig_data;
use circuit::witness::utils::WitnessBuilder;
use circuit::witness::withdraw::apply_withdraw_tx;
use circuit::witness::withdraw::calculate_withdraw_operations_from_witness;
use models::circuit::account::CircuitAccount;
use models::circuit::CircuitAccountTree;
use models::config_options::ThreadPanicNotify;
use models::node::{apply_updates, AccountMap};
use models::node::{Fr, FranklinOp};
use plasma::state::CollectedFee;
use prover::prover_data::ProverData;

pub struct ProversDataPool {
    last_prepared: i64,
    last_loaded: i64,
    limit: i64,
    operations: HashMap<i64, models::Operation>,
    prepared: HashMap<i64, ProverData>,
}

impl ProversDataPool {
    pub fn new() -> Self {
        ProversDataPool {
            last_prepared: 0,
            last_loaded: 0,
            limit: 10,
            operations: HashMap::new(),
            prepared: HashMap::new(),
        }
    }

    pub fn get(&self, block: i64) -> Option<&ProverData> {
        self.prepared.get(&block)
    }

    pub fn clean_up(&mut self, block: i64) {
        self.prepared.remove(&block);
    }

    fn has_capacity(&self) -> bool {
        self.last_loaded - self.last_prepared + (self.prepared.len() as i64) < self.limit as i64
    }

    fn all_prepared(&self) -> bool {
        self.operations.is_empty()
    }

    fn store_to_prove(&mut self, op: models::Operation) {
        let block = op.block.block_number as i64;
        self.last_loaded = block;
        self.operations.insert(block, op);
    }

    fn take_next_to_prove(&mut self) -> Result<models::Operation, String> {
        let mut first_from_loaded = 0;
        for key in self.operations.keys() {
            if first_from_loaded == 0 || *key < first_from_loaded {
                first_from_loaded = *key;
            }
        }
        match self.operations.remove(&first_from_loaded) {
            Some(v) => Ok(v),
            None => Err("data is inconsistent".to_owned()),
        }
    }
}

/// `Maintainer` is a helper structure that maintains the
/// prover data pool.
///
/// The essential part of this structure is `maintain` function
/// which runs forever and adds data to the externally owned
/// pool.
///
/// `migrate` function is private and is invoked by the
/// public `start` function, which starts
/// the named thread dedicated for that routine only.
pub struct Maintainer {
    /// Connection to the database.
    conn_pool: storage::ConnectionPool,
    /// Thread-safe reference to the data pool.
    data: Arc<RwLock<ProversDataPool>>,
    /// Routine refresh interval.
    rounds_interval: time::Duration,
    /// Cached account state.
    ///
    /// This field is initialized at the first iteration of `maintain`
    /// routine, and is updated by applying the state diff after that.
    account_state: Option<(u32, AccountMap)>,
}

impl Maintainer {
    /// Creates a new `Maintainer` object.
    pub fn new(
        conn_pool: storage::ConnectionPool,
        data: Arc<RwLock<ProversDataPool>>,
        rounds_interval: time::Duration,
    ) -> Self {
        Self {
            conn_pool,
            data,
            rounds_interval,
            account_state: None,
        }
    }

    /// Starts the thread running `maintain` method.
    pub fn start(mut self, panic_notify: mpsc::Sender<bool>) {
        thread::Builder::new()
            .name("prover_server_pool".to_string())
            .spawn(move || {
                let _panic_sentinel = ThreadPanicNotify(panic_notify);
                self.maintain();
            })
            .expect("failed to start provers server");
    }

    /// Updates the pool data in an infinite loop, awaiting `rounds_interval` time
    /// between updates.
    fn maintain(&mut self) {
        info!("preparing prover data routine started");
        loop {
            if self.has_capacity() {
                self.take_next_commits()
                    .expect("failed to get next commit operations");
            }
            self.prepare_next().expect("failed to prepare prover data");
            thread::sleep(self.rounds_interval);
        }
    }

    fn has_capacity(&self) -> bool {
        let data = self.data.read().expect("failed to acquire a lock");
        data.has_capacity()
    }

    fn take_next_commits(&self) -> Result<(), String> {
        let ops = {
            let data = self.data.read().expect("failed to acquire a lock");
            let storage = self
                .conn_pool
                .access_storage()
                .expect("failed to connect to db");
            storage
                .load_unverified_commits_after_block(data.last_loaded, data.limit)
                .map_err(|e| format!("failed to read commit operations: {}", e))?
        };

        if !ops.is_empty() {
            let mut data = self.data.write().expect("failed to acquire a lock");
            for op in ops.into_iter() {
                (*data).store_to_prove(op)
            }
        }

        Ok(())
    }

    fn prepare_next(&mut self) -> Result<(), String> {
        let op = {
            let mut data = self.data.write().expect("failed to acquire a lock");
            if data.all_prepared() {
                return Ok(());
            }
            data.take_next_to_prove()?
        };
        let storage = self
            .conn_pool
            .access_storage()
            .expect("failed to connect to db");
        let pd = self.build_prover_data(&storage, &op)?;
        let mut data = self.data.write().expect("failed to acquire a lock");
        (*data).last_prepared += 1;
        (*data).prepared.insert(op.block.block_number as i64, pd);
        Ok(())
    }

    /// Updates stored account state, obtaining the state for the requested block.
    ///
    /// This method updates the stored version of state with a diff, or initializes
    /// the state if it was not initialized yet.
    fn update_account_state(
        &mut self,
        storage: &storage::StorageProcessor,
        new_block: u32,
    ) -> Result<(), String> {
        match self.account_state {
            Some((block, ref state)) => {
                // State is initialized. We need to load diff (if any) and update
                // the stored state.
                let state_diff = storage
                    .load_state_diff(block, Some(new_block))
                    .map_err(|e| format!("failed to load committed state: {}", e))?;

                if let Some((_, state_diff)) = state_diff {
                    // Diff exists, update the state and return it.
                    let mut new_state = state.clone();

                    apply_updates(&mut new_state, state_diff);
                    debug!("Prover state is updated ({} => {})", block, new_block);

                    self.account_state = Some((new_block, new_state));
                }
            }
            None => {
                // State is not initialized, load it.
                let (block, accounts) = storage
                    .load_committed_state(Some(new_block))
                    .map_err(|e| format!("failed to load committed state: {}", e))?;

                debug!("Prover state is initialized");

                self.account_state = Some((block, accounts));
            }
        };

        Ok(())
    }

    /// Builds an `CircutAccountTree` based on the stored account state.
    ///
    /// This method does not update the account state itself and expects
    /// it to be up to date.
    fn build_account_tree(&self) -> CircuitAccountTree {
        assert!(
            self.account_state.is_some(),
            "There is no state to build a circuit account tree"
        );

        let mut account_tree = CircuitAccountTree::new(models::params::account_tree_depth() as u32);

        if let Some((_, ref state)) = self.account_state {
            for (&account_id, account) in state {
                let circuit_account = CircuitAccount::from(account.clone());
                account_tree.insert(account_id, circuit_account);
            }
        }

        account_tree
    }

    fn build_prover_data(
        &mut self,
        storage: &storage::StorageProcessor,
        commit_operation: &models::Operation,
    ) -> Result<ProverData, String> {
        let block_number = commit_operation.block.block_number;

        info!("building prover data for block {}", &block_number);

        self.update_account_state(storage, block_number - 1)?;
        let account_tree = self.build_account_tree();

        let mut witness_accum = WitnessBuilder::new(
            account_tree,
            commit_operation.block.fee_account,
            block_number,
        );

        let initial_root = witness_accum.account_tree.root_hash();
        let ops = storage
            .get_block_operations(block_number)
            .map_err(|e| format!("failed to get block operations {}", e))?;

        let mut operations = vec![];
        let mut pub_data = vec![];
        let mut fees = vec![];
        for op in ops {
            match op {
                FranklinOp::Deposit(deposit) => {
                    let deposit_witness =
                        apply_deposit_tx(&mut witness_accum.account_tree, &deposit);

                    let deposit_operations =
                        calculate_deposit_operations_from_witness(&deposit_witness);
                    operations.extend(deposit_operations);
                    pub_data.extend(deposit_witness.get_pubdata());
                }
                FranklinOp::Transfer(transfer) => {
                    let transfer_witness =
                        apply_transfer_tx(&mut witness_accum.account_tree, &transfer);

                    let sig_packed = transfer
                        .tx
                        .signature
                        .signature
                        .serialize_packed()
                        .map_err(|e| format!("failed to pack transaction signature {}", e))?;

                    let (
                        first_sig_msg,
                        second_sig_msg,
                        third_sig_msg,
                        signature_data,
                        signer_packed_key_bits,
                    ) = prepare_sig_data(
                        &sig_packed,
                        &transfer.tx.get_bytes(),
                        &transfer.tx.signature.pub_key,
                    )?;

                    let transfer_operations = calculate_transfer_operations_from_witness(
                        &transfer_witness,
                        &first_sig_msg,
                        &second_sig_msg,
                        &third_sig_msg,
                        &signature_data,
                        &signer_packed_key_bits,
                    );

                    operations.extend(transfer_operations);
                    fees.push(CollectedFee {
                        token: transfer.tx.token,
                        amount: transfer.tx.fee,
                    });
                    pub_data.extend(transfer_witness.get_pubdata());
                }
                FranklinOp::TransferToNew(transfer_to_new) => {
                    let transfer_to_new_witness =
                        apply_transfer_to_new_tx(&mut witness_accum.account_tree, &transfer_to_new);

                    let sig_packed = transfer_to_new
                        .tx
                        .signature
                        .signature
                        .serialize_packed()
                        .map_err(|e| format!("failed to pack transaction signature {}", e))?;

                    let (
                        first_sig_msg,
                        second_sig_msg,
                        third_sig_msg,
                        signature_data,
                        signer_packed_key_bits,
                    ) = prepare_sig_data(
                        &sig_packed,
                        &transfer_to_new.tx.get_bytes(),
                        &transfer_to_new.tx.signature.pub_key,
                    )?;

                    let transfer_to_new_operations =
                        calculate_transfer_to_new_operations_from_witness(
                            &transfer_to_new_witness,
                            &first_sig_msg,
                            &second_sig_msg,
                            &third_sig_msg,
                            &signature_data,
                            &signer_packed_key_bits,
                        );

                    operations.extend(transfer_to_new_operations);
                    fees.push(CollectedFee {
                        token: transfer_to_new.tx.token,
                        amount: transfer_to_new.tx.fee,
                    });
                    pub_data.extend(transfer_to_new_witness.get_pubdata());
                }
                FranklinOp::Withdraw(withdraw) => {
                    let withdraw_witness =
                        apply_withdraw_tx(&mut witness_accum.account_tree, &withdraw);

                    let sig_packed = withdraw
                        .tx
                        .signature
                        .signature
                        .serialize_packed()
                        .map_err(|e| format!("failed to pack transaction signature {}", e))?;

                    let (
                        first_sig_msg,
                        second_sig_msg,
                        third_sig_msg,
                        signature_data,
                        signer_packed_key_bits,
                    ) = prepare_sig_data(
                        &sig_packed,
                        &withdraw.tx.get_bytes(),
                        &withdraw.tx.signature.pub_key,
                    )?;

                    let withdraw_operations = calculate_withdraw_operations_from_witness(
                        &withdraw_witness,
                        &first_sig_msg,
                        &second_sig_msg,
                        &third_sig_msg,
                        &signature_data,
                        &signer_packed_key_bits,
                    );

                    operations.extend(withdraw_operations);
                    fees.push(CollectedFee {
                        token: withdraw.tx.token,
                        amount: withdraw.tx.fee,
                    });
                    pub_data.extend(withdraw_witness.get_pubdata());
                }
                FranklinOp::Close(close) => {
                    let close_account_witness =
                        apply_close_account_tx(&mut witness_accum.account_tree, &close);

                    let sig_packed = close
                        .tx
                        .signature
                        .signature
                        .serialize_packed()
                        .map_err(|e| format!("failed to pack signature: {}", e))?;

                    let (
                        first_sig_msg,
                        second_sig_msg,
                        third_sig_msg,
                        signature_data,
                        signer_packed_key_bits,
                    ) = prepare_sig_data(
                        &sig_packed,
                        &close.tx.get_bytes(),
                        &close.tx.signature.pub_key,
                    )?;

                    let close_account_operations = calculate_close_account_operations_from_witness(
                        &close_account_witness,
                        &first_sig_msg,
                        &second_sig_msg,
                        &third_sig_msg,
                        &signature_data,
                        &signer_packed_key_bits,
                    );

                    operations.extend(close_account_operations);
                    pub_data.extend(close_account_witness.get_pubdata());
                }
                FranklinOp::FullExit(full_exit_op) => {
                    let success = full_exit_op.withdraw_amount.is_some();

                    let full_exit_witness =
                        apply_full_exit_tx(&mut witness_accum.account_tree, &full_exit_op, success);

                    let full_exit_operations =
                        calculate_full_exit_operations_from_witness(&full_exit_witness);

                    operations.extend(full_exit_operations);
                    pub_data.extend(full_exit_witness.get_pubdata());
                }
                FranklinOp::ChangePubKeyOffchain(change_pkhash_op) => {
                    let change_pkhash_witness = apply_change_pubkey_offchain_tx(
                        &mut witness_accum.account_tree,
                        &change_pkhash_op,
                    );

                    let change_pkhash_operations =
                        calculate_change_pubkey_offchain_from_witness(&change_pkhash_witness);

                    operations.extend(change_pkhash_operations);
                    pub_data.extend(change_pkhash_witness.get_pubdata());
                }
                FranklinOp::Noop(_) => {} // Noops are handled below
            }
        }

        witness_accum.add_operation_with_pubdata(operations, pub_data);
        witness_accum.extend_pubdata_with_noops();
        assert_eq!(
            witness_accum.pubdata.len(),
            64 * models::params::block_size_chunks()
        );
        assert_eq!(
            witness_accum.operations.len(),
            models::params::block_size_chunks()
        );

        witness_accum.collect_fees(&fees);
        assert_eq!(
            witness_accum
                .root_after_fees
                .expect("root_after_fees not present"),
            commit_operation.block.new_root_hash
        );
        witness_accum.calculate_pubdata_commitment();

        Ok(ProverData {
            public_data_commitment: witness_accum.pubdata_commitment.unwrap(),
            old_root: initial_root,
            new_root: commit_operation.block.new_root_hash,
            validator_address: Fr::from_str(&commit_operation.block.fee_account.to_string())
                .expect("failed to parse"),
            operations: witness_accum.operations,
            validator_balances: witness_accum.fee_account_balances.unwrap(),
            validator_audit_path: witness_accum.fee_account_audit_path.unwrap(),
            validator_account: witness_accum.fee_account_witness.unwrap(),
        })
    }
}