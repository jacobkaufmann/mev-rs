use crate::payload::{builder::PayloadBuilder, job::PayloadJob};
use ethereum_consensus::clock::duration_until;
use reth::{
    api::PayloadBuilderAttributes,
    payload::{self, database::CachedReads, PayloadBuilderError},
    primitives::{
        revm_primitives::{Bytes, B256},
        BlockNumberOrTag,
    },
    providers::{BlockReaderIdExt, BlockSource, CanonStateNotification, StateProviderFactory},
    tasks::TaskSpawner,
    transaction_pool::TransactionPool,
};
use reth_basic_payload_builder::{PayloadConfig, PayloadTaskGuard, PrecachedState};
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone)]
pub struct PayloadJobGeneratorConfig {
    pub extradata: Bytes,
    // NOTE: currently ignored, see: https://github.com/paradigmxyz/reth/issues/7948
    pub _max_gas_limit: u64,
    pub interval: Duration,
    pub deadline: Duration,
    pub max_payload_tasks: usize,
}

#[derive(Debug)]
pub struct PayloadJobGenerator<Client, Pool, Tasks> {
    client: Client,
    pool: Pool,
    executor: Tasks,
    config: PayloadJobGeneratorConfig,
    payload_task_guard: PayloadTaskGuard,
    builder: PayloadBuilder,
    pre_cached: Option<PrecachedState>,
}

impl<Client, Pool, Tasks> PayloadJobGenerator<Client, Pool, Tasks> {
    pub fn with_builder(
        client: Client,
        pool: Pool,
        executor: Tasks,
        config: PayloadJobGeneratorConfig,
        builder: PayloadBuilder,
    ) -> Self {
        Self {
            client,
            pool,
            executor,
            payload_task_guard: PayloadTaskGuard::new(config.max_payload_tasks),
            config,
            builder,
            pre_cached: None,
        }
    }

    #[inline]
    fn max_job_duration(&self, unix_timestamp: u64) -> Duration {
        let duration_until_timestamp = duration_until(unix_timestamp);

        // safety in case clocks are bad
        let duration_until_timestamp = duration_until_timestamp.min(self.config.deadline * 3);

        self.config.deadline + duration_until_timestamp
    }

    #[inline]
    fn job_deadline(&self, unix_timestamp: u64) -> tokio::time::Instant {
        tokio::time::Instant::now() + self.max_job_duration(unix_timestamp)
    }

    fn maybe_pre_cached(&self, parent: B256) -> Option<CachedReads> {
        self.pre_cached.as_ref().filter(|pc| pc.block == parent).map(|pc| pc.cached.clone())
    }
}

impl<Client, Pool, Tasks> payload::PayloadJobGenerator for PayloadJobGenerator<Client, Pool, Tasks>
where
    Client: StateProviderFactory + BlockReaderIdExt + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + Unpin + 'static,
{
    type Job = PayloadJob<Client, Pool, Tasks>;

    fn new_payload_job(
        &self,
        attributes: <Self::Job as payload::PayloadJob>::PayloadAttributes,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let parent_block = if attributes.parent().is_zero() {
            // use latest block if parent is zero: genesis block
            self.client
                .block_by_number_or_tag(BlockNumberOrTag::Latest)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?
                .seal_slow()
        } else {
            let block = self
                .client
                .find_block_by_hash(attributes.parent(), BlockSource::Any)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?;

            // we already know the hash, so we can seal it
            block.seal(attributes.parent())
        };

        let until = if attributes.proposal.is_some() {
            self.job_deadline(attributes.timestamp())
        } else {
            // If there is no attached proposal, then terminate the payload job immediately
            tokio::time::Instant::now()
        };
        let deadline = Box::pin(tokio::time::sleep_until(until));

        let config =
            PayloadConfig::new(Arc::new(parent_block), self.config.extradata.clone(), attributes);

        let cached_reads = self.maybe_pre_cached(config.parent_block.hash());

        Ok(PayloadJob {
            config,
            client: self.client.clone(),
            pool: self.pool.clone(),
            executor: self.executor.clone(),
            deadline,
            interval: tokio::time::interval(self.config.interval),
            best_payload: None,
            pending_block: None,
            cached_reads,
            payload_task_guard: self.payload_task_guard.clone(),
            builder: self.builder.clone(),
            pending_bid_update: None,
        })
    }

    fn on_new_state(&mut self, new_state: CanonStateNotification) {
        let mut cached = CachedReads::default();

        // extract the state from the notification and put it into the cache
        let committed = new_state.committed();
        let execution_outcome = committed.execution_outcome();
        for (addr, acc) in execution_outcome.bundle_accounts_iter() {
            if let Some(info) = acc.info.clone() {
                // we want pre cache existing accounts and their storage
                // this only includes changed accounts and storage but is better than nothing
                let storage =
                    acc.storage.iter().map(|(key, slot)| (*key, slot.present_value)).collect();
                cached.insert_account(addr, info, storage);
            }
        }

        self.pre_cached = Some(PrecachedState { block: committed.tip().hash(), cached });
    }
}
