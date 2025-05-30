//! The `banking_stage` processes Transaction messages. It is intended to be used
//! to construct a software pipeline. The stage uses all available CPU cores and
//! can do its processing in parallel with signature verification on the GPU.

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    self::{
        committer::Committer,
        consumer::Consumer,
        decision_maker::{BufferedPacketsDecision, DecisionMaker},
        forwarder::Forwarder,
        latest_unprocessed_votes::{LatestUnprocessedVotes, VoteSource},
        leader_slot_metrics::LeaderSlotMetricsTracker,
        packet_receiver::PacketReceiver,
        qos_service::QosService,
        unprocessed_transaction_storage::UnprocessedTransactionStorage,
    },
    crate::{
        banking_stage::{
            consume_worker::ConsumeWorker,
            packet_deserializer::PacketDeserializer,
            transaction_scheduler::{
                prio_graph_scheduler::PrioGraphScheduler,
                scheduler_controller::SchedulerController, scheduler_error::SchedulerError,
            },
        },
        bundle_stage::bundle_account_locker::BundleAccountLocker,
        validator::{BlockProductionMethod, TransactionStructure},
    },
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender},
    histogram::Histogram,
    solana_client::connection_cache::ConnectionCache,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfoQuery},
    solana_ledger::blockstore_processor::TransactionStatusSender,
    solana_measure::measure_us,
    solana_perf::{data_budget::DataBudget, packet::PACKETS_PER_BATCH},
    solana_poh::poh_recorder::{PohRecorder, TransactionRecorder},
    solana_runtime::{
        bank::Bank, bank_forks::BankForks, prioritization_fee_cache::PrioritizationFeeCache,
        vote_sender_types::ReplayVoteSender,
    },
    solana_sdk::{pubkey::Pubkey, timing::AtomicInterval},
    std::{
        cmp,
        collections::HashSet,
        env,
        ops::Deref,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    transaction_scheduler::{
        greedy_scheduler::{GreedyScheduler, GreedySchedulerConfig},
        prio_graph_scheduler::PrioGraphSchedulerConfig,
        receive_and_buffer::{
            ReceiveAndBuffer, SanitizedTransactionReceiveAndBuffer, TransactionViewReceiveAndBuffer,
        },
        transaction_state_container::TransactionStateContainer,
    },
};

// Below modules are pub to allow use by banking_stage bench
pub mod committer;
pub mod consumer;
pub mod forwarder;
pub mod leader_slot_metrics;
pub mod qos_service;
pub mod unprocessed_packet_batches;
pub mod unprocessed_transaction_storage;

mod consume_worker;
pub(crate) mod decision_maker;
mod forward_packet_batches_by_accounts;
pub(crate) mod immutable_deserialized_packet;
mod latest_unprocessed_votes;
pub(crate) mod leader_slot_timing_metrics;
mod multi_iterator_scanner;
mod packet_deserializer;
pub(crate) mod packet_filter;
mod packet_receiver;
mod read_write_account_set;
mod scheduler_messages;
mod transaction_scheduler;

// proc_macro_hygiene needs to be stabilzied to use qualifier_attr...
// error[E0658]: non-inline modules in proc macro input are unstable
#[cfg(not(feature = "dev-context-only-utils"))]
pub(crate) mod unified_scheduler;
#[cfg(feature = "dev-context-only-utils")]
pub mod unified_scheduler;

// Fixed thread size seems to be fastest on GCP setup
pub const NUM_THREADS: u32 = 6;

pub const DEFAULT_BATCH_INTERVAL: Duration = Duration::from_millis(50);

const TOTAL_BUFFERED_PACKETS: usize = 100_000;

const NUM_VOTE_PROCESSING_THREADS: u32 = 2;
const MIN_THREADS_BANKING: u32 = 1;
const MIN_TOTAL_THREADS: u32 = NUM_VOTE_PROCESSING_THREADS + MIN_THREADS_BANKING;

const SLOT_BOUNDARY_CHECK_PERIOD: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
pub struct BankingStageStats {
    last_report: AtomicInterval,
    id: String,
    receive_and_buffer_packets_count: AtomicUsize,
    dropped_packets_count: AtomicUsize,
    pub(crate) dropped_duplicated_packets_count: AtomicUsize,
    dropped_forward_packets_count: AtomicUsize,
    newly_buffered_packets_count: AtomicUsize,
    newly_buffered_forwarded_packets_count: AtomicUsize,
    current_buffered_packets_count: AtomicUsize,
    rebuffered_packets_count: AtomicUsize,
    consumed_buffered_packets_count: AtomicUsize,
    forwarded_transaction_count: AtomicUsize,
    forwarded_vote_count: AtomicUsize,
    batch_packet_indexes_len: Histogram,

    // Timing
    consume_buffered_packets_elapsed: AtomicU64,
    receive_and_buffer_packets_elapsed: AtomicU64,
    filter_pending_packets_elapsed: AtomicU64,
    pub(crate) packet_conversion_elapsed: AtomicU64,
    transaction_processing_elapsed: AtomicU64,
}

impl BankingStageStats {
    pub fn new(id: u32) -> Self {
        BankingStageStats {
            id: id.to_string(),
            batch_packet_indexes_len: Histogram::configure()
                .max_value(PACKETS_PER_BATCH as u64)
                .build()
                .unwrap(),
            ..BankingStageStats::default()
        }
    }

    fn is_empty(&self) -> bool {
        0 == self
            .receive_and_buffer_packets_count
            .load(Ordering::Relaxed) as u64
            + self.dropped_packets_count.load(Ordering::Relaxed) as u64
            + self
                .dropped_duplicated_packets_count
                .load(Ordering::Relaxed) as u64
            + self.dropped_forward_packets_count.load(Ordering::Relaxed) as u64
            + self.newly_buffered_packets_count.load(Ordering::Relaxed) as u64
            + self.current_buffered_packets_count.load(Ordering::Relaxed) as u64
            + self.rebuffered_packets_count.load(Ordering::Relaxed) as u64
            + self.consumed_buffered_packets_count.load(Ordering::Relaxed) as u64
            + self
                .consume_buffered_packets_elapsed
                .load(Ordering::Relaxed)
            + self
                .receive_and_buffer_packets_elapsed
                .load(Ordering::Relaxed)
            + self.filter_pending_packets_elapsed.load(Ordering::Relaxed)
            + self.packet_conversion_elapsed.load(Ordering::Relaxed)
            + self.transaction_processing_elapsed.load(Ordering::Relaxed)
            + self.forwarded_transaction_count.load(Ordering::Relaxed) as u64
            + self.forwarded_vote_count.load(Ordering::Relaxed) as u64
            + self.batch_packet_indexes_len.entries()
    }

    fn report(&mut self, report_interval_ms: u64) {
        // skip reporting metrics if stats is empty
        if self.is_empty() {
            return;
        }
        if self.last_report.should_update(report_interval_ms) {
            datapoint_info!(
                "banking_stage-loop-stats",
                "id" => self.id,
                (
                    "receive_and_buffer_packets_count",
                    self.receive_and_buffer_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "dropped_packets_count",
                    self.dropped_packets_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "dropped_duplicated_packets_count",
                    self.dropped_duplicated_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "dropped_forward_packets_count",
                    self.dropped_forward_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "newly_buffered_packets_count",
                    self.newly_buffered_packets_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "newly_buffered_forwarded_packets_count",
                    self.newly_buffered_forwarded_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "current_buffered_packets_count",
                    self.current_buffered_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "rebuffered_packets_count",
                    self.rebuffered_packets_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "consumed_buffered_packets_count",
                    self.consumed_buffered_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "forwarded_transaction_count",
                    self.forwarded_transaction_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "forwarded_vote_count",
                    self.forwarded_vote_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "consume_buffered_packets_elapsed",
                    self.consume_buffered_packets_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "receive_and_buffer_packets_elapsed",
                    self.receive_and_buffer_packets_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "filter_pending_packets_elapsed",
                    self.filter_pending_packets_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "packet_conversion_elapsed",
                    self.packet_conversion_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "transaction_processing_elapsed",
                    self.transaction_processing_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "packet_batch_indices_len_min",
                    self.batch_packet_indexes_len.minimum().unwrap_or(0),
                    i64
                ),
                (
                    "packet_batch_indices_len_max",
                    self.batch_packet_indexes_len.maximum().unwrap_or(0),
                    i64
                ),
                (
                    "packet_batch_indices_len_mean",
                    self.batch_packet_indexes_len.mean().unwrap_or(0),
                    i64
                ),
                (
                    "packet_batch_indices_len_90pct",
                    self.batch_packet_indexes_len.percentile(90.0).unwrap_or(0),
                    i64
                )
            );
            self.batch_packet_indexes_len.clear();
        }
    }
}

#[derive(Debug, Default)]
pub struct BatchedTransactionDetails {
    pub costs: BatchedTransactionCostDetails,
    pub errors: BatchedTransactionErrorDetails,
}

#[derive(Debug, Default)]
pub struct BatchedTransactionCostDetails {
    pub batched_signature_cost: u64,
    pub batched_write_lock_cost: u64,
    pub batched_data_bytes_cost: u64,
    pub batched_loaded_accounts_data_size_cost: u64,
    pub batched_programs_execute_cost: u64,
}

#[derive(Debug, Default)]
pub struct BatchedTransactionErrorDetails {
    pub batched_retried_txs_per_block_limit_count: u64,
    pub batched_retried_txs_per_vote_limit_count: u64,
    pub batched_retried_txs_per_account_limit_count: u64,
    pub batched_retried_txs_per_account_data_block_limit_count: u64,
    pub batched_dropped_txs_per_account_data_total_limit_count: u64,
}

/// Stores the stage's thread handle and output receiver.
pub struct BankingStage {
    bank_thread_hdls: Vec<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub enum ForwardOption {
    NotForward,
    ForwardTpuVote,
    ForwardTransaction,
}

#[derive(Debug, Default)]
pub struct FilterForwardingResults {
    pub(crate) total_forwardable_packets: usize,
    pub(crate) total_dropped_packets: usize,
    pub(crate) total_packet_conversion_us: u64,
    pub(crate) total_filter_packets_us: u64,
}

pub trait LikeClusterInfo: Send + Sync + 'static + Clone {
    fn id(&self) -> Pubkey;

    fn lookup_contact_info<R>(&self, id: &Pubkey, query: impl ContactInfoQuery<R>) -> Option<R>;
}

impl LikeClusterInfo for Arc<ClusterInfo> {
    fn id(&self) -> Pubkey {
        self.deref().id()
    }

    fn lookup_contact_info<R>(&self, id: &Pubkey, query: impl ContactInfoQuery<R>) -> Option<R> {
        self.deref().lookup_contact_info(id, query)
    }
}

impl BankingStage {
    /// Create the stage using `bank`. Exit when `verified_receiver` is dropped.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_production_method: BlockProductionMethod,
        transaction_struct: TransactionStructure,
        cluster_info: &impl LikeClusterInfo,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        non_vote_receiver: BankingPacketReceiver,
        tpu_vote_receiver: BankingPacketReceiver,
        gossip_vote_receiver: BankingPacketReceiver,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: ReplayVoteSender,
        log_messages_bytes_limit: Option<usize>,
        connection_cache: Arc<ConnectionCache>,
        bank_forks: Arc<RwLock<BankForks>>,
        prioritization_fee_cache: &Arc<PrioritizationFeeCache>,
        enable_forwarding: bool,
        blacklisted_accounts: HashSet<Pubkey>,
        bundle_account_locker: BundleAccountLocker,
        // callback function for compute space reservation for BundleStage
        block_cost_limit_block_cost_limit_reservation_cb: impl Fn(&Bank) -> u64 + Clone + Send + 'static,
        batch_interval: Duration,
    ) -> Self {
        Self::new_num_threads(
            block_production_method,
            transaction_struct,
            cluster_info,
            poh_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            Self::num_threads(),
            transaction_status_sender,
            replay_vote_sender,
            log_messages_bytes_limit,
            connection_cache,
            bank_forks,
            prioritization_fee_cache,
            enable_forwarding,
            blacklisted_accounts,
            bundle_account_locker,
            block_cost_limit_block_cost_limit_reservation_cb,
            batch_interval,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_num_threads(
        block_production_method: BlockProductionMethod,
        transaction_struct: TransactionStructure,
        cluster_info: &impl LikeClusterInfo,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        non_vote_receiver: BankingPacketReceiver,
        tpu_vote_receiver: BankingPacketReceiver,
        gossip_vote_receiver: BankingPacketReceiver,
        num_threads: u32,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: ReplayVoteSender,
        log_messages_bytes_limit: Option<usize>,
        connection_cache: Arc<ConnectionCache>,
        bank_forks: Arc<RwLock<BankForks>>,
        prioritization_fee_cache: &Arc<PrioritizationFeeCache>,
        enable_forwarding: bool,
        blacklisted_accounts: HashSet<Pubkey>,
        bundle_account_locker: BundleAccountLocker,
        block_cost_limit_reservation_cb: impl Fn(&Bank) -> u64 + Clone + Send + 'static,
        batch_interval: Duration,
    ) -> Self {
        match block_production_method {
            BlockProductionMethod::CentralScheduler
            | BlockProductionMethod::CentralSchedulerGreedy => {
                let use_greedy_scheduler = matches!(
                    block_production_method,
                    BlockProductionMethod::CentralSchedulerGreedy
                );
                Self::new_central_scheduler(
                    transaction_struct,
                    use_greedy_scheduler,
                    cluster_info,
                    poh_recorder,
                    non_vote_receiver,
                    tpu_vote_receiver,
                    gossip_vote_receiver,
                    num_threads,
                    transaction_status_sender,
                    replay_vote_sender,
                    log_messages_bytes_limit,
                    connection_cache,
                    bank_forks,
                    prioritization_fee_cache,
                    enable_forwarding,
                    blacklisted_accounts,
                    bundle_account_locker,
                    block_cost_limit_reservation_cb,
                    batch_interval,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_central_scheduler(
        transaction_struct: TransactionStructure,
        use_greedy_scheduler: bool,
        cluster_info: &impl LikeClusterInfo,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        non_vote_receiver: BankingPacketReceiver,
        tpu_vote_receiver: BankingPacketReceiver,
        gossip_vote_receiver: BankingPacketReceiver,
        num_threads: u32,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: ReplayVoteSender,
        log_messages_bytes_limit: Option<usize>,
        connection_cache: Arc<ConnectionCache>,
        bank_forks: Arc<RwLock<BankForks>>,
        prioritization_fee_cache: &Arc<PrioritizationFeeCache>,
        enable_forwarding: bool,
        blacklisted_accounts: HashSet<Pubkey>,
        bundle_account_locker: BundleAccountLocker,
        block_cost_limit_reservation_cb: impl Fn(&Bank) -> u64 + Clone + Send + 'static,
        batch_interval: Duration,
    ) -> Self {
        assert!(num_threads >= MIN_TOTAL_THREADS);
        // Single thread to generate entries from many banks.
        // This thread talks to poh_service and broadcasts the entries once they have been recorded.
        // Once an entry has been recorded, its blockhash is registered with the bank.
        let data_budget = Arc::new(DataBudget::default());
        // Keeps track of extraneous vote transactions for the vote threads
        let latest_unprocessed_votes = {
            let bank = bank_forks.read().unwrap().working_bank();
            Arc::new(LatestUnprocessedVotes::new(&bank))
        };

        let decision_maker = DecisionMaker::new(cluster_info.id(), poh_recorder.clone());
        let committer = Committer::new(
            transaction_status_sender.clone(),
            replay_vote_sender.clone(),
            prioritization_fee_cache.clone(),
        );
        let transaction_recorder = poh_recorder.read().unwrap().new_recorder();

        // + 1 for the central scheduler thread
        let mut bank_thread_hdls = Vec::with_capacity(num_threads as usize + 1);

        // Spawn legacy voting threads first: 1 gossip, 1 tpu
        for (id, packet_receiver, vote_source) in [
            (0, gossip_vote_receiver, VoteSource::Gossip),
            (1, tpu_vote_receiver, VoteSource::Tpu),
        ] {
            bank_thread_hdls.push(Self::spawn_thread_local_multi_iterator_thread(
                id,
                packet_receiver,
                decision_maker.clone(),
                committer.clone(),
                transaction_recorder.clone(),
                log_messages_bytes_limit,
                Forwarder::new(
                    poh_recorder.clone(),
                    bank_forks.clone(),
                    cluster_info.clone(),
                    connection_cache.clone(),
                    data_budget.clone(),
                ),
                UnprocessedTransactionStorage::new_vote_storage(
                    latest_unprocessed_votes.clone(),
                    vote_source,
                ),
                blacklisted_accounts.clone(),
                bundle_account_locker.clone(),
                block_cost_limit_reservation_cb.clone(),
            ));
        }

        let transaction_struct =
            if enable_forwarding && !matches!(transaction_struct, TransactionStructure::Sdk) {
                warn!(
                "Forwarding only supported for `Sdk` transaction struct. Overriding to use `Sdk`."
            );
                TransactionStructure::Sdk
            } else {
                transaction_struct
            };

        match transaction_struct {
            TransactionStructure::Sdk => {
                let receive_and_buffer = SanitizedTransactionReceiveAndBuffer::new(
                    PacketDeserializer::new(non_vote_receiver),
                    bank_forks.clone(),
                    enable_forwarding,
                    batch_interval,
                );
                Self::spawn_scheduler_and_workers(
                    &mut bank_thread_hdls,
                    receive_and_buffer,
                    use_greedy_scheduler,
                    decision_maker,
                    committer,
                    cluster_info,
                    poh_recorder,
                    num_threads,
                    log_messages_bytes_limit,
                    connection_cache,
                    bank_forks,
                    enable_forwarding,
                    data_budget,
                    blacklisted_accounts.clone(),
                    bundle_account_locker.clone(),
                    block_cost_limit_reservation_cb.clone(),
                );
            }
            TransactionStructure::View => {
                let receive_and_buffer = TransactionViewReceiveAndBuffer {
                    receiver: non_vote_receiver,
                    bank_forks: bank_forks.clone(),
                };
                Self::spawn_scheduler_and_workers(
                    &mut bank_thread_hdls,
                    receive_and_buffer,
                    use_greedy_scheduler,
                    decision_maker,
                    committer,
                    cluster_info,
                    poh_recorder,
                    num_threads,
                    log_messages_bytes_limit,
                    connection_cache,
                    bank_forks,
                    enable_forwarding,
                    data_budget,
                    blacklisted_accounts.clone(),
                    bundle_account_locker.clone(),
                    block_cost_limit_reservation_cb.clone(),
                );
            }
        }

        Self { bank_thread_hdls }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_scheduler_and_workers<R: ReceiveAndBuffer + Send + Sync + 'static>(
        bank_thread_hdls: &mut Vec<JoinHandle<()>>,
        receive_and_buffer: R,
        use_greedy_scheduler: bool,
        decision_maker: DecisionMaker,
        committer: Committer,
        cluster_info: &impl LikeClusterInfo,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
        num_threads: u32,
        log_messages_bytes_limit: Option<usize>,
        connection_cache: Arc<ConnectionCache>,
        bank_forks: Arc<RwLock<BankForks>>,
        enable_forwarding: bool,
        data_budget: Arc<DataBudget>,
        blacklisted_accounts: HashSet<Pubkey>,
        bundle_account_locker: BundleAccountLocker,
        block_cost_limit_reservation_cb: impl Fn(&Bank) -> u64 + Clone + Send + 'static,
    ) {
        // Create channels for communication between scheduler and workers
        let num_workers = (num_threads).saturating_sub(NUM_VOTE_PROCESSING_THREADS);
        let (work_senders, work_receivers): (Vec<Sender<_>>, Vec<Receiver<_>>) =
            (0..num_workers).map(|_| unbounded()).unzip();
        let (finished_work_sender, finished_work_receiver) = unbounded();

        // Spawn the worker threads
        let mut worker_metrics = Vec::with_capacity(num_workers as usize);
        for (index, work_receiver) in work_receivers.into_iter().enumerate() {
            let id = (index as u32).saturating_add(NUM_VOTE_PROCESSING_THREADS);
            let consume_worker = ConsumeWorker::new(
                id,
                work_receiver,
                Consumer::new(
                    committer.clone(),
                    poh_recorder.read().unwrap().new_recorder(),
                    QosService::new(id),
                    log_messages_bytes_limit,
                    blacklisted_accounts.clone(),
                    bundle_account_locker.clone(),
                ),
                finished_work_sender.clone(),
                poh_recorder.read().unwrap().new_leader_bank_notifier(),
            );

            worker_metrics.push(consume_worker.metrics_handle());
            let cb = block_cost_limit_reservation_cb.clone();
            bank_thread_hdls.push(
                Builder::new()
                    .name(format!("solCoWorker{id:02}"))
                    .spawn(move || {
                        let _ = consume_worker.run(cb);
                    })
                    .unwrap(),
            )
        }

        let forwarder = enable_forwarding.then(|| {
            Forwarder::new(
                poh_recorder.clone(),
                bank_forks.clone(),
                cluster_info.clone(),
                connection_cache.clone(),
                data_budget.clone(),
            )
        });

        // Spawn the central scheduler thread
        if use_greedy_scheduler {
            bank_thread_hdls.push(
                Builder::new()
                    .name("solBnkTxSched".to_string())
                    .spawn(move || {
                        let scheduler = GreedyScheduler::new(
                            work_senders,
                            finished_work_receiver,
                            GreedySchedulerConfig::default(),
                        );
                        let scheduler_controller = SchedulerController::new(
                            decision_maker.clone(),
                            receive_and_buffer,
                            bank_forks,
                            scheduler,
                            worker_metrics,
                            forwarder,
                            blacklisted_accounts.clone(),
                        );

                        match scheduler_controller.run() {
                            Ok(_) => {}
                            Err(SchedulerError::DisconnectedRecvChannel(_)) => {}
                            Err(SchedulerError::DisconnectedSendChannel(_)) => {
                                warn!("Unexpected worker disconnect from scheduler")
                            }
                        }
                    })
                    .unwrap(),
            );
        } else {
            bank_thread_hdls.push(
                Builder::new()
                    .name("solBnkTxSched".to_string())
                    .spawn(move || {
                        let scheduler = PrioGraphScheduler::new(
                            work_senders,
                            finished_work_receiver,
                            PrioGraphSchedulerConfig::default(),
                        );
                        let scheduler_controller = SchedulerController::new(
                            decision_maker.clone(),
                            receive_and_buffer,
                            bank_forks,
                            scheduler,
                            worker_metrics,
                            forwarder,
                            blacklisted_accounts.clone(),
                        );

                        match scheduler_controller.run() {
                            Ok(_) => {}
                            Err(SchedulerError::DisconnectedRecvChannel(_)) => {}
                            Err(SchedulerError::DisconnectedSendChannel(_)) => {
                                warn!("Unexpected worker disconnect from scheduler")
                            }
                        }
                    })
                    .unwrap(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_thread_local_multi_iterator_thread<T: LikeClusterInfo>(
        id: u32,
        packet_receiver: BankingPacketReceiver,
        mut decision_maker: DecisionMaker,
        committer: Committer,
        transaction_recorder: TransactionRecorder,
        log_messages_bytes_limit: Option<usize>,
        mut forwarder: Forwarder<T>,
        unprocessed_transaction_storage: UnprocessedTransactionStorage,
        blacklisted_accounts: HashSet<Pubkey>,
        bundle_account_locker: BundleAccountLocker,
        block_cost_limit_reservation_cb: impl Fn(&Bank) -> u64 + Clone + Send + 'static,
    ) -> JoinHandle<()> {
        let mut packet_receiver = PacketReceiver::new(id, packet_receiver);
        let consumer = Consumer::new(
            committer,
            transaction_recorder,
            QosService::new(id),
            log_messages_bytes_limit,
            blacklisted_accounts.clone(),
            bundle_account_locker.clone(),
        );

        Builder::new()
            .name(format!("solBanknStgTx{id:02}"))
            .spawn(move || {
                Self::process_loop(
                    &mut packet_receiver,
                    &mut decision_maker,
                    &mut forwarder,
                    &consumer,
                    id,
                    unprocessed_transaction_storage,
                    block_cost_limit_reservation_cb,
                )
            })
            .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn process_buffered_packets<T: LikeClusterInfo>(
        decision_maker: &mut DecisionMaker,
        forwarder: &mut Forwarder<T>,
        consumer: &Consumer,
        unprocessed_transaction_storage: &mut UnprocessedTransactionStorage,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        block_cost_limit_reservation_cb: &impl Fn(&Bank) -> u64,
    ) {
        if unprocessed_transaction_storage.should_not_process() {
            return;
        }
        let (decision, make_decision_us) =
            measure_us!(decision_maker.make_consume_or_forward_decision());
        let metrics_action = slot_metrics_tracker.check_leader_slot_boundary(
            decision.bank_start(),
            Some(unprocessed_transaction_storage),
        );
        slot_metrics_tracker.increment_make_decision_us(make_decision_us);

        match decision {
            BufferedPacketsDecision::Consume(bank_start) => {
                // Take metrics action before consume packets (potentially resetting the
                // slot metrics tracker to the next slot) so that we don't count the
                // packet processing metrics from the next slot towards the metrics
                // of the previous slot
                slot_metrics_tracker.apply_action(metrics_action);
                let (_, consume_buffered_packets_us) = measure_us!(consumer
                    .consume_buffered_packets(
                        &bank_start,
                        unprocessed_transaction_storage,
                        banking_stage_stats,
                        slot_metrics_tracker,
                        block_cost_limit_reservation_cb
                    ));
                slot_metrics_tracker
                    .increment_consume_buffered_packets_us(consume_buffered_packets_us);
            }
            BufferedPacketsDecision::Forward => {
                let ((), forward_us) = measure_us!(forwarder.handle_forwarding(
                    unprocessed_transaction_storage,
                    false,
                    slot_metrics_tracker,
                    banking_stage_stats,
                ));
                slot_metrics_tracker.increment_forward_us(forward_us);
                // Take metrics action after forwarding packets to include forwarded
                // metrics into current slot
                slot_metrics_tracker.apply_action(metrics_action);
            }
            BufferedPacketsDecision::ForwardAndHold => {
                let ((), forward_and_hold_us) = measure_us!(forwarder.handle_forwarding(
                    unprocessed_transaction_storage,
                    true,
                    slot_metrics_tracker,
                    banking_stage_stats,
                ));
                slot_metrics_tracker.increment_forward_and_hold_us(forward_and_hold_us);
                // Take metrics action after forwarding packets
                slot_metrics_tracker.apply_action(metrics_action);
            }
            _ => (),
        }
    }

    fn process_loop<T: LikeClusterInfo>(
        packet_receiver: &mut PacketReceiver,
        decision_maker: &mut DecisionMaker,
        forwarder: &mut Forwarder<T>,
        consumer: &Consumer,
        id: u32,
        mut unprocessed_transaction_storage: UnprocessedTransactionStorage,
        block_cost_limit_reservation_cb: impl Fn(&Bank) -> u64,
    ) {
        let mut banking_stage_stats = BankingStageStats::new(id);

        let mut slot_metrics_tracker = LeaderSlotMetricsTracker::new(id);
        let mut last_metrics_update = Instant::now();

        loop {
            if !unprocessed_transaction_storage.is_empty()
                || last_metrics_update.elapsed() >= SLOT_BOUNDARY_CHECK_PERIOD
            {
                let (_, process_buffered_packets_us) = measure_us!(Self::process_buffered_packets(
                    decision_maker,
                    forwarder,
                    consumer,
                    &mut unprocessed_transaction_storage,
                    &banking_stage_stats,
                    &mut slot_metrics_tracker,
                    &block_cost_limit_reservation_cb
                ));
                slot_metrics_tracker
                    .increment_process_buffered_packets_us(process_buffered_packets_us);
                last_metrics_update = Instant::now();
            }

            match packet_receiver.receive_and_buffer_packets(
                &mut unprocessed_transaction_storage,
                &mut banking_stage_stats,
                &mut slot_metrics_tracker,
            ) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => break,
            }
            banking_stage_stats.report(1000);
        }
    }

    pub fn num_threads() -> u32 {
        cmp::max(
            env::var("SOLANA_BANKING_THREADS")
                .map(|x| x.parse().unwrap_or(NUM_THREADS))
                .unwrap_or(NUM_THREADS),
            MIN_TOTAL_THREADS,
        )
    }

    pub fn join(self) -> thread::Result<()> {
        for bank_thread_hdl in self.bank_thread_hdls {
            bank_thread_hdl.join()?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) fn update_bank_forks_and_poh_recorder_for_new_tpu_bank(
    bank_forks: &RwLock<BankForks>,
    poh_recorder: &RwLock<PohRecorder>,
    tpu_bank: Bank,
    track_transaction_indexes: bool,
) {
    let tpu_bank = bank_forks.write().unwrap().insert(tpu_bank);
    poh_recorder
        .write()
        .unwrap()
        .set_bank(tpu_bank, track_transaction_indexes);
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_trace::{BankingTracer, Channels},
        agave_banking_stage_ingress_types::BankingPacketBatch,
        crossbeam_channel::{unbounded, Receiver},
        itertools::Itertools,
        solana_entry::entry::{self, EntrySlice},
        solana_gossip::cluster_info::Node,
        solana_ledger::{
            blockstore::Blockstore,
            genesis_utils::{
                create_genesis_config, create_genesis_config_with_leader, GenesisConfigInfo,
            },
            get_tmp_ledger_path_auto_delete,
            leader_schedule_cache::LeaderScheduleCache,
        },
        solana_perf::packet::{to_packet_batches, PacketBatch},
        solana_poh::{
            poh_recorder::{
                create_test_recorder, PohRecorderError, Record, RecordTransactionsSummary,
                WorkingBankEntry,
            },
            poh_service::PohService,
        },
        solana_runtime::{bank::Bank, genesis_utils::bootstrap_validator_stake_lamports},
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_sdk::{
            hash::Hash,
            poh_config::PohConfig,
            pubkey::Pubkey,
            signature::{Keypair, Signer},
            system_transaction,
            transaction::{SanitizedTransaction, Transaction},
        },
        solana_streamer::socket::SocketAddrSpace,
        solana_vote_program::{
            vote_state::TowerSync, vote_transaction::new_tower_sync_transaction,
        },
        std::{
            sync::atomic::{AtomicBool, Ordering},
            thread::sleep,
        },
        strum::IntoEnumIterator,
        test_case::test_case,
    };

    pub(crate) fn new_test_cluster_info(keypair: Option<Arc<Keypair>>) -> (Node, ClusterInfo) {
        let keypair = keypair.unwrap_or_else(|| Arc::new(Keypair::new()));
        let node = Node::new_localhost_with_pubkey(&keypair.pubkey());
        let cluster_info =
            ClusterInfo::new(node.info.clone(), keypair, SocketAddrSpace::Unspecified);
        (node, cluster_info)
    }

    pub(crate) fn sanitize_transactions(
        txs: Vec<Transaction>,
    ) -> Vec<RuntimeTransaction<SanitizedTransaction>> {
        txs.into_iter()
            .map(RuntimeTransaction::from_transaction_for_tests)
            .collect()
    }

    #[test_case(TransactionStructure::Sdk)]
    #[test_case(TransactionStructure::View)]
    fn test_banking_stage_shutdown1(transaction_struct: TransactionStructure) {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let blockstore = Arc::new(
                Blockstore::open(ledger_path.path())
                    .expect("Expected to be able to open database ledger"),
            );
            let (exit, poh_recorder, poh_service, _entry_receiever) =
                create_test_recorder(bank, blockstore, None, None);
            let (_, cluster_info) = new_test_cluster_info(/*keypair:*/ None);
            let cluster_info = Arc::new(cluster_info);
            let (replay_vote_sender, _replay_vote_receiver) = unbounded();

            let banking_stage = BankingStage::new(
                BlockProductionMethod::CentralScheduler,
                transaction_struct,
                &cluster_info,
                &poh_recorder,
                non_vote_receiver,
                tpu_vote_receiver,
                gossip_vote_receiver,
                None,
                replay_vote_sender,
                None,
                Arc::new(ConnectionCache::new("connection_cache_test")),
                bank_forks,
                &Arc::new(PrioritizationFeeCache::new(0u64)),
                false,
                HashSet::default(),
                BundleAccountLocker::default(),
                |_| 0,
                Duration::ZERO,
            );
            drop(non_vote_sender);
            drop(tpu_vote_sender);
            drop(gossip_vote_sender);
            exit.store(true, Ordering::Relaxed);
            banking_stage.join().unwrap();
            poh_service.join().unwrap();
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }

    #[test_case(TransactionStructure::Sdk)]
    #[test_case(TransactionStructure::View)]
    fn test_banking_stage_tick(transaction_struct: TransactionStructure) {
        solana_logger::setup();
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        genesis_config.ticks_per_slot = 4;
        let num_extra_ticks = 2;
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let blockstore = Arc::new(
                Blockstore::open(ledger_path.path())
                    .expect("Expected to be able to open database ledger"),
            );
            let poh_config = PohConfig {
                target_tick_count: Some(bank.max_tick_height() + num_extra_ticks),
                ..PohConfig::default()
            };
            let (exit, poh_recorder, poh_service, entry_receiver) =
                create_test_recorder(bank.clone(), blockstore, Some(poh_config), None);
            let (_, cluster_info) = new_test_cluster_info(/*keypair:*/ None);
            let cluster_info = Arc::new(cluster_info);
            let (replay_vote_sender, _replay_vote_receiver) = unbounded();

            let banking_stage = BankingStage::new(
                BlockProductionMethod::CentralScheduler,
                transaction_struct,
                &cluster_info,
                &poh_recorder,
                non_vote_receiver,
                tpu_vote_receiver,
                gossip_vote_receiver,
                None,
                replay_vote_sender,
                None,
                Arc::new(ConnectionCache::new("connection_cache_test")),
                bank_forks,
                &Arc::new(PrioritizationFeeCache::new(0u64)),
                false,
                HashSet::default(),
                BundleAccountLocker::default(),
                |_| 0,
                Duration::ZERO,
            );
            trace!("sending bank");
            drop(non_vote_sender);
            drop(tpu_vote_sender);
            drop(gossip_vote_sender);
            exit.store(true, Ordering::Relaxed);
            poh_service.join().unwrap();
            drop(poh_recorder);

            trace!("getting entries");
            let entries: Vec<_> = entry_receiver
                .iter()
                .flat_map(
                    |WorkingBankEntry {
                         bank: _,
                         entries_ticks,
                     }| entries_ticks.into_iter().map(|(e, _)| e),
                )
                .collect();
            trace!("done");
            assert_eq!(entries.len(), genesis_config.ticks_per_slot as usize);
            assert!(entries.verify(&start_hash, &entry::thread_pool_for_tests()));
            assert_eq!(entries[entries.len() - 1].hash, bank.last_blockhash());
            banking_stage.join().unwrap();
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }

    pub fn convert_from_old_verified(
        mut with_vers: Vec<(PacketBatch, Vec<u8>)>,
    ) -> Vec<PacketBatch> {
        with_vers.iter_mut().for_each(|(b, v)| {
            b.iter_mut()
                .zip(v)
                .for_each(|(p, f)| p.meta_mut().set_discard(*f == 0))
        });
        with_vers.into_iter().map(|(b, _)| b).collect()
    }

    fn test_banking_stage_entries_only(
        block_production_method: BlockProductionMethod,
        transaction_struct: TransactionStructure,
    ) {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let blockstore = Arc::new(
                Blockstore::open(ledger_path.path())
                    .expect("Expected to be able to open database ledger"),
            );
            let (exit, poh_recorder, poh_service, entry_receiver) =
                create_test_recorder(bank.clone(), blockstore, None, None);
            let (_, cluster_info) = new_test_cluster_info(/*keypair:*/ None);
            let cluster_info = Arc::new(cluster_info);
            let (replay_vote_sender, _replay_vote_receiver) = unbounded();

            let banking_stage = BankingStage::new(
                block_production_method,
                transaction_struct,
                &cluster_info,
                &poh_recorder,
                non_vote_receiver,
                tpu_vote_receiver,
                gossip_vote_receiver,
                None,
                replay_vote_sender,
                None,
                Arc::new(ConnectionCache::new("connection_cache_test")),
                bank_forks.clone(), // keep a local-copy of bank-forks so worker threads do not lose weak access to bank-forks
                &Arc::new(PrioritizationFeeCache::new(0u64)),
                false,
                HashSet::default(),
                BundleAccountLocker::default(),
                |_| 0,
                Duration::ZERO,
            );

            // fund another account so we can send 2 good transactions in a single batch.
            let keypair = Keypair::new();
            let fund_tx =
                system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 2, start_hash);
            bank.process_transaction(&fund_tx).unwrap();

            // good tx
            let to = solana_pubkey::new_rand();
            let tx = system_transaction::transfer(&mint_keypair, &to, 1, start_hash);

            // good tx, but no verify
            let to2 = solana_pubkey::new_rand();
            let tx_no_ver = system_transaction::transfer(&keypair, &to2, 2, start_hash);

            // bad tx, AccountNotFound
            let keypair = Keypair::new();
            let to3 = solana_pubkey::new_rand();
            let tx_anf = system_transaction::transfer(&keypair, &to3, 1, start_hash);

            // send 'em over
            let packet_batches = to_packet_batches(&[tx_no_ver, tx_anf, tx], 3);

            // glad they all fit
            assert_eq!(packet_batches.len(), 1);

            let packet_batches = packet_batches
                .into_iter()
                .map(|batch| (batch, vec![0u8, 1u8, 1u8]))
                .collect();
            let packet_batches = convert_from_old_verified(packet_batches);
            non_vote_sender // no_ver, anf, tx
                .send(BankingPacketBatch::new(packet_batches))
                .unwrap();

            drop(non_vote_sender);
            drop(tpu_vote_sender);
            drop(gossip_vote_sender);
            // wait until banking_stage to finish up all packets
            banking_stage.join().unwrap();

            exit.store(true, Ordering::Relaxed);
            poh_service.join().unwrap();
            drop(poh_recorder);

            let mut blockhash = start_hash;
            let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
            bank.process_transaction(&fund_tx).unwrap();
            //receive entries + ticks
            loop {
                let entries: Vec<_> = entry_receiver
                    .iter()
                    .flat_map(
                        |WorkingBankEntry {
                             bank: _,
                             entries_ticks,
                         }| entries_ticks.into_iter().map(|(e, _)| e),
                    )
                    .collect();

                assert!(entries.verify(&blockhash, &entry::thread_pool_for_tests()));
                if !entries.is_empty() {
                    blockhash = entries.last().unwrap().hash;
                    for entry in entries {
                        bank.process_entry_transactions(entry.transactions)
                            .iter()
                            .for_each(|x| assert_eq!(*x, Ok(())));
                    }
                }

                if bank.get_balance(&to) == 1 {
                    break;
                }

                sleep(Duration::from_millis(200));
            }

            assert_eq!(bank.get_balance(&to), 1);
            assert_eq!(bank.get_balance(&to2), 0);

            drop(entry_receiver);
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }

    #[test_case(TransactionStructure::Sdk)]
    #[test_case(TransactionStructure::View)]
    fn test_banking_stage_entries_only_central_scheduler(transaction_struct: TransactionStructure) {
        test_banking_stage_entries_only(
            BlockProductionMethod::CentralScheduler,
            transaction_struct,
        );
    }

    #[test_case(TransactionStructure::Sdk)]
    #[test_case(TransactionStructure::View)]
    fn test_banking_stage_entryfication(transaction_struct: TransactionStructure) {
        solana_logger::setup();
        // In this attack we'll demonstrate that a verifier can interpret the ledger
        // differently if either the server doesn't signal the ledger to add an
        // Entry OR if the verifier tries to parallelize across multiple Entries.
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(2);
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);

        // Process a batch that includes a transaction that receives two lamports.
        let alice = Keypair::new();
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 2, genesis_config.hash());

        let packet_batches = to_packet_batches(&[tx], 1);
        let packet_batches = packet_batches
            .into_iter()
            .map(|batch| (batch, vec![1u8]))
            .collect();
        let packet_batches = convert_from_old_verified(packet_batches);
        non_vote_sender
            .send(BankingPacketBatch::new(packet_batches))
            .unwrap();

        // Process a second batch that uses the same from account, so conflicts with above TX
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 1, genesis_config.hash());
        let packet_batches = to_packet_batches(&[tx], 1);
        let packet_batches = packet_batches
            .into_iter()
            .map(|batch| (batch, vec![1u8]))
            .collect();
        let packet_batches = convert_from_old_verified(packet_batches);
        non_vote_sender
            .send(BankingPacketBatch::new(packet_batches))
            .unwrap();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let (replay_vote_sender, _replay_vote_receiver) = unbounded();

            let entry_receiver = {
                // start a banking_stage to eat verified receiver
                let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
                let blockstore = Arc::new(
                    Blockstore::open(ledger_path.path())
                        .expect("Expected to be able to open database ledger"),
                );
                let (exit, poh_recorder, poh_service, entry_receiver) =
                    create_test_recorder(bank.clone(), blockstore, None, None);
                let (_, cluster_info) = new_test_cluster_info(/*keypair:*/ None);
                let cluster_info = Arc::new(cluster_info);
                let _banking_stage = BankingStage::new(
                    BlockProductionMethod::CentralScheduler,
                    transaction_struct,
                    &cluster_info,
                    &poh_recorder,
                    non_vote_receiver,
                    tpu_vote_receiver,
                    gossip_vote_receiver,
                    None,
                    replay_vote_sender,
                    None,
                    Arc::new(ConnectionCache::new("connection_cache_test")),
                    bank_forks,
                    &Arc::new(PrioritizationFeeCache::new(0u64)),
                    false,
                    HashSet::default(),
                    BundleAccountLocker::default(),
                    |_| 0,
                    Duration::ZERO,
                );

                // wait for banking_stage to eat the packets
                while bank.get_balance(&alice.pubkey()) < 1 {
                    sleep(Duration::from_millis(10));
                }
                exit.store(true, Ordering::Relaxed);
                poh_service.join().unwrap();
                entry_receiver
            };
            drop(non_vote_sender);
            drop(tpu_vote_sender);
            drop(gossip_vote_sender);

            // consume the entire entry_receiver, feed it into a new bank
            // check that the balance is what we expect.
            let entries: Vec<_> = entry_receiver
                .iter()
                .flat_map(
                    |WorkingBankEntry {
                         bank: _,
                         entries_ticks,
                     }| entries_ticks.into_iter().map(|e| e.0),
                )
                .collect();

            let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
            for entry in entries {
                bank.process_entry_transactions(entry.transactions)
                    .iter()
                    .for_each(|x| assert_eq!(*x, Ok(())));
            }

            // Assert the user doesn't hold three lamports. If the stage only outputs one
            // entry, then one of the transactions will be rejected, because it drives
            // the account balance below zero before the credit is added.
            assert!(bank.get_balance(&alice.pubkey()) != 3);
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }

    #[test]
    fn test_bank_record_transactions() {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let blockstore = Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger");
            let (poh_recorder, entry_receiver, record_receiver) = PohRecorder::new(
                // TODO use record_receiver
                bank.tick_height(),
                bank.last_blockhash(),
                bank.clone(),
                None,
                bank.ticks_per_slot(),
                Arc::new(blockstore),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &PohConfig::default(),
                Arc::new(AtomicBool::default()),
            );
            let recorder = poh_recorder.new_recorder();
            let poh_recorder = Arc::new(RwLock::new(poh_recorder));

            let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

            poh_recorder
                .write()
                .unwrap()
                .set_bank_for_test(bank.clone());
            let pubkey = solana_pubkey::new_rand();
            let keypair2 = Keypair::new();
            let pubkey2 = solana_pubkey::new_rand();

            let txs = vec![
                system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash())
                    .into(),
                system_transaction::transfer(&keypair2, &pubkey2, 1, genesis_config.hash()).into(),
            ];

            let _ = recorder.record_transactions(bank.slot(), vec![txs.clone()]);
            let WorkingBankEntry {
                bank,
                entries_ticks,
            } = entry_receiver.recv().unwrap();
            assert_eq!(entries_ticks.len(), 1);
            let entry = &entries_ticks[0].0;
            assert_eq!(entry.transactions, txs);

            // Once bank is set to a new bank (setting bank.slot() + 1 in record_transactions),
            // record_transactions should throw MaxHeightReached
            let next_slot = bank.slot() + 1;
            let RecordTransactionsSummary { result, .. } =
                recorder.record_transactions(next_slot, vec![txs]);
            assert_matches!(result, Err(PohRecorderError::MaxHeightReached));
            // Should receive nothing from PohRecorder b/c record failed
            assert!(entry_receiver.try_recv().is_err());

            poh_recorder
                .read()
                .unwrap()
                .is_exited
                .store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }

    pub(crate) fn create_slow_genesis_config(lamports: u64) -> GenesisConfigInfo {
        create_slow_genesis_config_with_leader(lamports, &solana_pubkey::new_rand())
    }

    pub(crate) fn create_slow_genesis_config_with_leader(
        lamports: u64,
        validator_pubkey: &Pubkey,
    ) -> GenesisConfigInfo {
        let mut config_info = create_genesis_config_with_leader(
            lamports,
            validator_pubkey,
            // See solana_ledger::genesis_utils::create_genesis_config.
            bootstrap_validator_stake_lamports(),
        );

        // For these tests there's only 1 slot, don't want to run out of ticks
        config_info.genesis_config.ticks_per_slot *= 1024;
        config_info
    }

    pub(crate) fn simulate_poh(
        record_receiver: Receiver<Record>,
        poh_recorder: &Arc<RwLock<PohRecorder>>,
    ) -> JoinHandle<()> {
        let poh_recorder = poh_recorder.clone();
        let is_exited = poh_recorder.read().unwrap().is_exited.clone();
        let tick_producer = Builder::new()
            .name("solana-simulate_poh".to_string())
            .spawn(move || loop {
                PohService::read_record_receiver_and_process(
                    &poh_recorder,
                    &record_receiver,
                    Duration::from_millis(10),
                );
                if is_exited.load(Ordering::Relaxed) {
                    break;
                }
            });
        tick_producer.unwrap()
    }

    #[test_case(TransactionStructure::Sdk)]
    #[test_case(TransactionStructure::View)]
    fn test_unprocessed_transaction_storage_full_send(transaction_struct: TransactionStructure) {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10000);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        {
            let blockstore = Arc::new(
                Blockstore::open(ledger_path.path())
                    .expect("Expected to be able to open database ledger"),
            );
            let (exit, poh_recorder, poh_service, _entry_receiver) =
                create_test_recorder(bank.clone(), blockstore, None, None);
            let (_, cluster_info) = new_test_cluster_info(/*keypair:*/ None);
            let cluster_info = Arc::new(cluster_info);
            let (replay_vote_sender, _replay_vote_receiver) = unbounded();

            let banking_stage = BankingStage::new(
                BlockProductionMethod::CentralScheduler,
                transaction_struct,
                &cluster_info,
                &poh_recorder,
                non_vote_receiver,
                tpu_vote_receiver,
                gossip_vote_receiver,
                None,
                replay_vote_sender,
                None,
                Arc::new(ConnectionCache::new("connection_cache_test")),
                bank_forks,
                &Arc::new(PrioritizationFeeCache::new(0u64)),
                false,
                HashSet::default(),
                BundleAccountLocker::default(),
                |_| 0,
                Duration::ZERO,
            );

            let keypairs = (0..100).map(|_| Keypair::new()).collect_vec();
            let vote_keypairs = (0..100).map(|_| Keypair::new()).collect_vec();
            for keypair in keypairs.iter() {
                bank.process_transaction(&system_transaction::transfer(
                    &mint_keypair,
                    &keypair.pubkey(),
                    20,
                    start_hash,
                ))
                .unwrap();
            }

            // Send a bunch of votes and transfers
            let tpu_votes = (0..100_usize)
                .map(|i| {
                    new_tower_sync_transaction(
                        TowerSync::from(vec![
                            (0, 8),
                            (1, 7),
                            (i as u64 + 10, 6),
                            (i as u64 + 11, 1),
                        ]),
                        Hash::new_unique(),
                        &keypairs[i],
                        &vote_keypairs[i],
                        &vote_keypairs[i],
                        None,
                    )
                })
                .collect_vec();
            let gossip_votes = (0..100_usize)
                .map(|i| {
                    new_tower_sync_transaction(
                        TowerSync::from(vec![
                            (0, 9),
                            (1, 8),
                            (i as u64 + 5, 6),
                            (i as u64 + 63, 1),
                        ]),
                        Hash::new_unique(),
                        &keypairs[i],
                        &vote_keypairs[i],
                        &vote_keypairs[i],
                        None,
                    )
                })
                .collect_vec();
            let txs = (0..100_usize)
                .map(|i| {
                    system_transaction::transfer(
                        &keypairs[i],
                        &keypairs[(i + 1) % 100].pubkey(),
                        10,
                        start_hash,
                    );
                })
                .collect_vec();

            let non_vote_packet_batches = to_packet_batches(&txs, 10);
            let tpu_packet_batches = to_packet_batches(&tpu_votes, 10);
            let gossip_packet_batches = to_packet_batches(&gossip_votes, 10);

            // Send em all
            [
                (non_vote_packet_batches, non_vote_sender),
                (tpu_packet_batches, tpu_vote_sender),
                (gossip_packet_batches, gossip_vote_sender),
            ]
            .into_iter()
            .map(|(packet_batches, sender)| {
                Builder::new()
                    .spawn(move || {
                        sender
                            .send(BankingPacketBatch::new(packet_batches))
                            .unwrap()
                    })
                    .unwrap()
            })
            .for_each(|handle| handle.join().unwrap());

            banking_stage.join().unwrap();
            exit.store(true, Ordering::Relaxed);
            poh_service.join().unwrap();
        }
        Blockstore::destroy(ledger_path.path()).unwrap();
    }

    #[test]
    fn test_blacklisted_accounts() {
        solana_logger::setup();

        for block_production_method in BlockProductionMethod::iter() {
            let GenesisConfigInfo {
                genesis_config,
                mint_keypair,
                ..
            } = create_slow_genesis_config(10);
            let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
            let start_hash = bank.last_blockhash();
            let banking_tracer = BankingTracer::new_disabled();
            let Channels {
                non_vote_sender,
                non_vote_receiver,
                tpu_vote_sender,
                tpu_vote_receiver,
                gossip_vote_sender,
                gossip_vote_receiver,
            } = banking_tracer.create_channels(false);

            let ledger_path = get_tmp_ledger_path_auto_delete!();
            {
                let blockstore = Arc::new(
                    Blockstore::open(ledger_path.path())
                        .expect("Expected to be able to open database ledger"),
                );
                let (exit, poh_recorder, poh_service, entry_receiver) =
                    create_test_recorder(bank.clone(), blockstore, None, None);
                let (_, cluster_info) = new_test_cluster_info(/*keypair:*/ None);
                let cluster_info = Arc::new(cluster_info);
                let (replay_vote_sender, _replay_vote_receiver) = unbounded();

                let blacklisted_keypair = Keypair::new();

                let banking_stage = BankingStage::new(
                    block_production_method,
                    TransactionStructure::default(),
                    &cluster_info,
                    &poh_recorder,
                    non_vote_receiver,
                    tpu_vote_receiver,
                    gossip_vote_receiver,
                    None,
                    replay_vote_sender,
                    None,
                    Arc::new(ConnectionCache::new("connection_cache_test")),
                    bank_forks.clone(), // keep a local-copy of bank-forks so worker threads do not lose weak access to bank-forks
                    &Arc::new(PrioritizationFeeCache::new(0u64)),
                    false,
                    HashSet::from_iter([blacklisted_keypair.pubkey()]),
                    BundleAccountLocker::default(),
                    |_| 0,
                    Duration::ZERO,
                );

                // bad tx
                let blacklisted_tx = system_transaction::transfer(
                    &mint_keypair,
                    &blacklisted_keypair.pubkey(),
                    2,
                    start_hash,
                );

                // good tx
                let good_keypair = Keypair::new();
                let ok_tx = system_transaction::transfer(
                    &mint_keypair,
                    &good_keypair.pubkey(),
                    2,
                    start_hash,
                );

                // send 'em over
                let packet_batches = to_packet_batches(&[blacklisted_tx.clone(), ok_tx.clone()], 2);

                // glad they all fit
                assert_eq!(packet_batches.len(), 1);
                non_vote_sender
                    .send(BankingPacketBatch::new(packet_batches))
                    .unwrap();

                // wait for 512 ticks or 8 leader slots to pass before checking state
                while let Ok(msg) = entry_receiver.recv() {
                    if msg.entries_ticks.iter().any(|(_e, tick)| *tick == 511) {
                        break;
                    }
                }

                drop(non_vote_sender);
                drop(tpu_vote_sender);
                drop(gossip_vote_sender);
                exit.store(true, Ordering::Relaxed);
                poh_service.join().unwrap();
                banking_stage.join().unwrap();

                assert_eq!(bank.get_balance(&good_keypair.pubkey()), 2);
                assert!(bank.has_signature(&ok_tx.signatures[0]));

                assert_eq!(bank.get_balance(&blacklisted_keypair.pubkey()), 0);
                assert!(!bank.has_signature(&blacklisted_tx.signatures[0]));
            }
            Blockstore::destroy(ledger_path.path()).unwrap();
        }
    }
}
