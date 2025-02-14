// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! An interactive cluster server.

use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Error};
use async_trait::async_trait;
use differential_dataflow::trace::ExertionLogic;
use futures::future;
use mz_cluster_client::client::{ClusterStartupEpoch, TimelyConfig};
use mz_ore::error::ErrorExt;
use mz_ore::halt;
use mz_service::client::{GenericClient, Partitionable, Partitioned};
use mz_service::local::{LocalActivator, LocalClient};
use timely::communication::initialize::WorkerGuards;
use timely::execute::execute_from;
use timely::WorkerConfig;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::communication::initialize_networking;

type PartitionedClient<C, R> = Partitioned<LocalClient<C, R>, C, R>;

/// A client managing access to the local portion of a Timely cluster
pub struct ClusterClient<Client, Worker, C, R>
where
    Worker: crate::types::AsRunnableWorker<C, R>,
{
    /// The actual client to talk to the cluster
    inner: Option<Client>,
    /// The running timely instance
    timely_container: TimelyContainerRef<C, R>,
    /// The handle to the Tokio runtime.
    tokio_handle: tokio::runtime::Handle,
    worker: Worker,
}

/// Metadata about timely workers in this process.
pub struct TimelyContainer<C, R> {
    /// The current timely config in use
    config: TimelyConfig,
    /// Channels over which to send endpoints for wiring up a new Client
    client_txs: Vec<
        crossbeam_channel::Sender<(
            crossbeam_channel::Receiver<C>,
            mpsc::UnboundedSender<R>,
            mpsc::UnboundedSender<LocalActivator>,
        )>,
    >,
    /// Thread guards that keep worker threads alive
    _worker_guards: WorkerGuards<()>,
}

impl<C, R> Drop for TimelyContainer<C, R> {
    fn drop(&mut self) {
        panic!("Timely container must never drop");
    }
}

/// Threadsafe reference to an optional TimelyContainer
type TimelyContainerRef<C, R> = Arc<tokio::sync::Mutex<Option<TimelyContainer<C, R>>>>;

impl<Worker, C, R> ClusterClient<PartitionedClient<C, R>, Worker, C, R>
where
    C: Send + 'static,
    R: Send + 'static,
    (C, R): Partitionable<C, R>,
    Worker: crate::types::AsRunnableWorker<C, R> + Clone + Send + Sync + 'static,
{
    /// Create a new `ClusterClient`.
    pub fn new(
        timely_container: TimelyContainerRef<C, R>,
        tokio_handle: tokio::runtime::Handle,
        worker_config: Worker,
    ) -> Self {
        Self {
            timely_container,
            inner: None,
            tokio_handle,
            worker: worker_config,
        }
    }

    async fn build_timely(
        user_worker_config: Worker,
        config: TimelyConfig,
        epoch: ClusterStartupEpoch,
        tokio_executor: Handle,
    ) -> Result<TimelyContainer<C, R>, Error> {
        info!("Building timely container with config {config:?}");
        let (client_txs, client_rxs): (Vec<_>, Vec<_>) = (0..config.workers)
            .map(|_| crossbeam_channel::unbounded())
            .unzip();
        let client_rxs: Mutex<Vec<_>> = Mutex::new(client_rxs.into_iter().map(Some).collect());

        let (builders, other) = initialize_networking(
            config.workers,
            config.process,
            config.addresses.clone(),
            epoch,
        )
        .await?;

        let mut worker_config = WorkerConfig::default();

        // We set a custom exertion logic for proportionality > 0. A proportionality value of 0
        // means that no arrangement merge effort is exerted and merging occurs only in response to
        // updates.
        if config.arrangement_exert_proportionality > 0 {
            let merge_effort = Some(1000);

            // ExertionLogic defines a function to determine if a spine is sufficiently tidied.
            // Its arguments are an iterator over the index of a layer, the count of batches in the
            // layer and the length of batches at the layer. The iterator enumerates layers from the
            // largest to the smallest layer.

            let arc: ExertionLogic = Arc::new(move |layers| {
                let mut prop = config.arrangement_exert_proportionality;

                // Layers are ordered from largest to smallest.
                // Skip to the largest occupied layer.
                let layers = layers
                    .iter()
                    .copied()
                    .skip_while(|(_idx, count, _len)| *count == 0);

                let mut first = true;
                for (_idx, count, len) in layers {
                    if count > 1 {
                        // Found an in-progress merge that we should continue.
                        return merge_effort;
                    }

                    if !first && prop > 0 && len > 0 {
                        // Found a non-empty batch within `arrangement_exert_proportionality` of
                        // the largest one.
                        return merge_effort;
                    }

                    first = false;
                    prop /= 2;
                }

                None
            });
            worker_config.set::<ExertionLogic>("differential/default_exert_logic".to_string(), arc);
        }

        let worker_guards = execute_from(builders, other, worker_config, move |timely_worker| {
            let timely_worker_index = timely_worker.index();
            let _tokio_guard = tokio_executor.enter();
            let client_rx = client_rxs.lock().unwrap()[timely_worker_index % config.workers]
                .take()
                .unwrap();
            let user_worker_config = user_worker_config.clone();
            Worker::build_and_run(user_worker_config, timely_worker, client_rx)
        })
        .map_err(|e| anyhow!("{e}"))?;

        Ok(TimelyContainer {
            config,
            client_txs,
            _worker_guards: worker_guards,
        })
    }

    async fn build(
        &mut self,
        config: TimelyConfig,
        epoch: ClusterStartupEpoch,
    ) -> Result<(), Error> {
        let workers = config.workers;

        // Check if we can reuse the existing timely instance.
        // We currently do not support reinstantiating timely, we simply panic if another config is
        // requested. This code must panic before dropping the worker guards contained in
        // timely_container. As we don't terminate timely workers, the thread join would hang
        // forever, possibly creating a fair share of confusion in the orchestrator.

        let handle = self.tokio_handle.clone();

        let worker_config = self.worker.clone();
        let mut timely_container = self.timely_container.lock().await;
        match &*timely_container {
            Some(existing) => {
                if config != existing.config {
                    halt!(
                        "new timely configuration does not match existing timely configuration:\n{:?}\nvs\n{:?}",
                        config,
                        existing.config,
                    );
                }
                info!("Timely already initialized; re-using.",);
            }
            None => {
                let timely = Self::build_timely(worker_config, config, epoch, handle)
                    .await
                    .map_err(|e| {
                        warn!("timely initialization failed: {}", e.display_with_causes());
                        e
                    })?;

                *timely_container = Some(timely);
            }
        };

        let timely = timely_container.as_ref().expect("set above");

        // Order is important here: If our future is canceled, we need to drop the `command_txs`
        // before the `activators` so when the workers are unparked by the dropping of the
        // activators they can observe that the senders have disconnected.
        let mut activators = Vec::with_capacity(workers);
        let mut command_txs = Vec::with_capacity(workers);
        let mut response_rxs = Vec::with_capacity(workers);
        let mut activator_rxs = Vec::with_capacity(workers);
        for client_tx in &timely.client_txs {
            let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
            let (resp_tx, resp_rx) = mpsc::unbounded_channel();
            let (activator_tx, activator_rx) = mpsc::unbounded_channel();

            client_tx
                .send((cmd_rx, resp_tx, activator_tx))
                .expect("worker not dropped");

            command_txs.push(cmd_tx);
            response_rxs.push(resp_rx);
            activator_rxs.push(activator_rx);
        }

        // It's important that we wait for activators only after we have sent the channels to all
        // workers. Otherwise we could end up in a stalled state. See database-issues#8957.
        for mut activator_rx in activator_rxs {
            let activator = activator_rx.recv().await.expect("worker not dropped");
            activators.push(activator);
        }

        self.inner = Some(LocalClient::new_partitioned(
            response_rxs,
            command_txs,
            activators,
        ));
        Ok(())
    }
}

impl<Client: Debug, Worker: crate::types::AsRunnableWorker<C, R>, C, R> Debug
    for ClusterClient<Client, Worker, C, R>
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterClient")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<Worker, C, R> GenericClient<C, R> for ClusterClient<PartitionedClient<C, R>, Worker, C, R>
where
    C: Send + Debug + mz_cluster_client::client::TryIntoTimelyConfig + 'static,
    R: Send + Debug + 'static,
    (C, R): Partitionable<C, R>,
    Worker: crate::types::AsRunnableWorker<C, R> + Send + Sync + Clone + 'static,
{
    async fn send(&mut self, cmd: C) -> Result<(), Error> {
        // Changing this debug statement requires changing the replica-isolation test
        tracing::debug!("ClusterClient send={:?}", &cmd);
        match cmd.try_into_timely_config() {
            Ok((config, epoch)) => self.build(config, epoch).await,
            Err(cmd) => self.inner.as_mut().expect("initialized").send(cmd).await,
        }
    }

    /// # Cancel safety
    ///
    /// This method is cancel safe. If `recv` is used as the event in a [`tokio::select!`]
    /// statement and some other branch completes first, it is guaranteed that no messages were
    /// received by this client.
    async fn recv(&mut self) -> Result<Option<R>, Error> {
        if let Some(client) = self.inner.as_mut() {
            // `Partitioned::recv` is documented as cancel safe.
            client.recv().await
        } else {
            future::pending().await
        }
    }
}
