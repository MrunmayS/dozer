use crate::dag::channels::ProcessorChannelForwarder;
use crate::dag::dag_metadata::SOURCE_ID_IDENTIFIER;
use crate::dag::epoch::{Epoch, EpochManager};
use crate::dag::errors::ExecutionError;
use crate::dag::errors::ExecutionError::{InternalError, InvalidPortHandle};
use crate::dag::executor::ExecutorOperation;
use crate::dag::executor_utils::StateOptions;
use crate::dag::node::{NodeHandle, PortHandle};
use crate::storage::common::Database;
use crate::storage::errors::StorageError::SerializationError;
use crate::storage::lmdb_storage::SharedTransaction;
use crossbeam::channel::Sender;
use dozer_types::internal_err;
use dozer_types::parking_lot::RwLock;
use dozer_types::types::{Operation, Record, Schema};
use log::info;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct StateWriter {
    meta_db: Database,
    dbs: HashMap<PortHandle, StateOptions>,
    output_schemas: HashMap<PortHandle, Schema>,
    input_schemas: HashMap<PortHandle, Schema>,
    input_ports: Option<Vec<PortHandle>>,
    tx: SharedTransaction,
}

impl StateWriter {
    pub fn new(
        meta_db: Database,
        dbs: HashMap<PortHandle, StateOptions>,
        tx: SharedTransaction,
        input_ports: Option<Vec<PortHandle>>,
        output_schemas: HashMap<PortHandle, Schema>,
        input_schemas: HashMap<PortHandle, Schema>,
    ) -> Self {
        Self {
            meta_db,
            dbs,
            output_schemas,
            input_schemas,
            tx,
            input_ports,
        }
    }

    fn write_record(
        db: Database,
        rec: &Record,
        schema: &Schema,
        tx: &SharedTransaction,
    ) -> Result<(), ExecutionError> {
        let key = rec.get_key(&schema.primary_index);
        let value = bincode::serialize(&rec).map_err(|e| SerializationError {
            typ: "Record".to_string(),
            reason: Box::new(e),
        })?;
        tx.write().put(db, key.as_slice(), value.as_slice())?;
        Ok(())
    }

    fn retr_record(
        db: Database,
        key: &[u8],
        tx: &SharedTransaction,
    ) -> Result<Record, ExecutionError> {
        let tx = tx.read();
        let curr = tx
            .get(db, key)?
            .ok_or_else(ExecutionError::RecordNotFound)?;

        let r: Record = bincode::deserialize(curr).map_err(|e| SerializationError {
            typ: "Record".to_string(),
            reason: Box::new(e),
        })?;
        Ok(r)
    }

    fn store_op(&mut self, op: Operation, port: &PortHandle) -> Result<Operation, ExecutionError> {
        if let Some(opts) = self.dbs.get(port) {
            let schema = self
                .output_schemas
                .get(port)
                .ok_or(ExecutionError::InvalidPortHandle(*port))?;

            match op {
                Operation::Insert { new } => {
                    StateWriter::write_record(opts.db, &new, schema, &self.tx)?;
                    Ok(Operation::Insert { new })
                }
                Operation::Delete { mut old } => {
                    let key = old.get_key(&schema.primary_index);
                    if opts.options.retrieve_old_record_for_deletes {
                        old = StateWriter::retr_record(opts.db, &key, &self.tx)?;
                    }
                    self.tx.write().del(opts.db, &key, None)?;
                    Ok(Operation::Delete { old })
                }
                Operation::Update { mut old, new } => {
                    let key = old.get_key(&schema.primary_index);
                    if opts.options.retrieve_old_record_for_updates {
                        old = StateWriter::retr_record(opts.db, &key, &self.tx)?;
                    }
                    StateWriter::write_record(opts.db, &new, schema, &self.tx)?;
                    Ok(Operation::Update { old, new })
                }
            }
        } else {
            Ok(op)
        }
    }

    pub fn store_commit_info(&mut self, epoch_details: &Epoch) -> Result<(), ExecutionError> {
        //
        for (source, (txid, seq_in_tx)) in &epoch_details.details {
            let mut full_key = vec![SOURCE_ID_IDENTIFIER];
            full_key.extend(source.to_bytes());

            let mut value: Vec<u8> = Vec::with_capacity(16);
            value.extend(txid.to_be_bytes());
            value.extend(seq_in_tx.to_be_bytes());

            self.tx
                .write()
                .put(self.meta_db, full_key.as_slice(), value.as_slice())?;
        }
        self.tx.write().commit_and_renew()?;
        Ok(())
    }

    pub(crate) fn get_all_input_schemas(&self) -> Option<HashMap<PortHandle, Schema>> {
        match &self.input_ports {
            Some(input_ports) => {
                let count = input_ports
                    .iter()
                    .filter(|e| !self.input_schemas.contains_key(*e))
                    .count();
                if count == 0 {
                    Some(self.input_schemas.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ChannelManager {
    owner: NodeHandle,
    senders: HashMap<PortHandle, Vec<Sender<ExecutorOperation>>>,
    state_writer: StateWriter,
    stateful: bool,
}

impl ChannelManager {
    #[inline]
    fn send_op(&mut self, mut op: Operation, port_id: PortHandle) -> Result<(), ExecutionError> {
        if self.stateful {
            op = self.state_writer.store_op(op, &port_id)?;
        }

        let senders = self
            .senders
            .get(&port_id)
            .ok_or(InvalidPortHandle(port_id))?;

        let exec_op = match op {
            Operation::Insert { new } => ExecutorOperation::Insert { new },
            Operation::Update { old, new } => ExecutorOperation::Update { old, new },
            Operation::Delete { old } => ExecutorOperation::Delete { old },
        };

        if senders.len() == 1 {
            internal_err!(senders[0].send(exec_op))?;
        } else {
            for sender in senders {
                internal_err!(sender.send(exec_op.clone()))?;
            }
        }

        Ok(())
    }

    pub fn send_term_and_wait(&self) -> Result<(), ExecutionError> {
        for senders in &self.senders {
            for sender in senders.1 {
                internal_err!(sender.send(ExecutorOperation::Terminate))?;
            }
        }

        Ok(())
    }

    pub fn store_and_send_commit(&mut self, epoch_details: &Epoch) -> Result<(), ExecutionError> {
        info!("[{}] Checkpointing - {}", self.owner, &epoch_details);
        self.state_writer.store_commit_info(epoch_details)?;

        for senders in &self.senders {
            for sender in senders.1 {
                internal_err!(sender.send(ExecutorOperation::Commit {
                    epoch_details: epoch_details.clone()
                }))?;
            }
        }

        Ok(())
    }
    pub fn new(
        owner: NodeHandle,
        senders: HashMap<PortHandle, Vec<Sender<ExecutorOperation>>>,
        state_writer: StateWriter,
        stateful: bool,
    ) -> Self {
        Self {
            owner,
            senders,
            state_writer,
            stateful,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SourceChannelManager {
    source_handle: NodeHandle,
    manager: ChannelManager,
    curr_txid: u64,
    curr_seq_in_tx: u64,
    epoch_manager: Arc<RwLock<EpochManager>>,
}

impl SourceChannelManager {
    pub fn new(
        owner: NodeHandle,
        senders: HashMap<PortHandle, Vec<Sender<ExecutorOperation>>>,
        state_writer: StateWriter,
        stateful: bool,
        epoch_manager: Arc<RwLock<EpochManager>>,
    ) -> Self {
        Self {
            manager: ChannelManager::new(owner.clone(), senders, state_writer, stateful),
            curr_txid: 0,
            curr_seq_in_tx: 0,
            source_handle: owner,
            epoch_manager,
        }
    }

    fn advance_and_trigger_commit_if_needed(
        &mut self,
        advance: u16,
        force: bool,
    ) -> Result<bool, ExecutionError> {
        let epoch = if let Some(epoch) =
            self.epoch_manager
                .write()
                .advance(&self.source_handle, advance, force)?
        {
            self.manager.store_and_send_commit(&Epoch::from(
                epoch.id,
                self.source_handle.clone(),
                self.curr_txid,
                self.curr_seq_in_tx,
            ))?;
            Some((epoch.barrier, epoch.forced))
        } else {
            None
        };

        if let Some((barrier, forced)) = epoch {
            barrier.wait();
            Ok(forced)
        } else {
            Ok(false)
        }
    }

    pub fn trigger_commit_if_needed(&mut self, force: bool) -> Result<bool, ExecutionError> {
        self.advance_and_trigger_commit_if_needed(0, force)
    }

    pub fn send_and_trigger_commit_if_needed(
        &mut self,
        txid: u64,
        seq_in_tx: u64,
        op: Operation,
        port: PortHandle,
        force: bool,
    ) -> Result<bool, ExecutionError> {
        //
        self.curr_txid = txid;
        self.curr_seq_in_tx = seq_in_tx;
        self.manager.send_op(op, port)?;
        self.advance_and_trigger_commit_if_needed(1, force)
    }

    pub fn terminate(&mut self) -> Result<(), ExecutionError> {
        self.manager.send_term_and_wait()
    }
}

#[derive(Debug)]
pub(crate) struct ProcessorChannelManager {
    manager: ChannelManager,
}

impl ProcessorChannelManager {
    pub fn new(
        owner: NodeHandle,
        senders: HashMap<PortHandle, Vec<Sender<ExecutorOperation>>>,
        state_writer: StateWriter,
        stateful: bool,
    ) -> Self {
        Self {
            manager: ChannelManager::new(owner, senders, state_writer, stateful),
        }
    }

    pub fn store_and_send_commit(&mut self, epoch_details: &Epoch) -> Result<(), ExecutionError> {
        self.manager.store_and_send_commit(epoch_details)
    }

    pub fn send_term_and_wait(&self) -> Result<(), ExecutionError> {
        self.manager.send_term_and_wait()
    }
}

impl ProcessorChannelForwarder for ProcessorChannelManager {
    fn send(&mut self, op: Operation, port: PortHandle) -> Result<(), ExecutionError> {
        self.manager.send_op(op, port)
    }
}
