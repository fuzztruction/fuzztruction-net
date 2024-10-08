use std::{
    fs::{self},
    sync::{atomic::AtomicBool, Arc, Barrier, Mutex, Once, RwLock},
};

use crate::{
    config::Config,
    constants::MAX_WORKER_RESTART_CNT,
    fuzzer::event_counter::FuzzerEventCounter,
    sink_bitmap::{Bitmap, BITMAP_DEFAULT_MAP_SIZE},
};

use anyhow::Result;
use log::*;

use super::{
    queue::Queue,
    worker::{FuzzingWorker, WorkerProxy, WorkerUid},
    worker_impl::Cerebrum,
};

/// A fuzzing campaign for a specific source / sink configuration.
#[derive(Debug)]
pub struct FuzzingCampaign {
    /// The configuration for the campaign.
    config: Config,
    /// The queue that is used by all workers to persist their fuzzing progress.
    queue: Arc<Mutex<Queue>>,
    /// A list of all workers that belong to the campaign.
    workers: Vec<WorkerProxy>,
    initialization_done: Arc<Once>,
    initialization_failed: Arc<AtomicBool>,
    cerebrum: Arc<RwLock<Option<Cerebrum>>>,
    shared_virgin_map: Arc<Mutex<Bitmap>>,
    shared_crash_virgin_map: Arc<Mutex<Bitmap>>,
    restarted_worker: Vec<WorkerUid>,
}

impl FuzzingCampaign {
    /// Create a new FuzzingCampaign based on the provided config.
    pub fn new(config: &Config) -> Result<Self> {
        let queue = Arc::new(Mutex::new(Queue::new()));
        let workers = Vec::new();

        let mut config_path = config.general.work_dir.clone();
        config_path.push("config.json");
        let config_json = serde_json::to_string_pretty(&config).unwrap();
        fs::write(&config_path, config_json).unwrap();

        let initialization_done = Arc::new(Once::new());
        let initialization_failed = Arc::new(AtomicBool::new(false));

        let cerebrum = Arc::new(RwLock::new(None));
        let shared_virgin_map = Arc::new(Mutex::new(Bitmap::new_in_mem(
            BITMAP_DEFAULT_MAP_SIZE,
            0xff,
        )));
        let shared_crash_virgin_map = Arc::new(Mutex::new(Bitmap::new_in_mem(
            BITMAP_DEFAULT_MAP_SIZE,
            0xff,
        )));

        Ok(FuzzingCampaign {
            config: config.clone(),
            queue,
            workers,
            initialization_done,
            initialization_failed,
            cerebrum,
            shared_virgin_map,
            shared_crash_virgin_map,
            restarted_worker: Vec::new(),
        })
    }

    /// Get the queue that is shared by all threads.
    pub fn queue(&self) -> Arc<Mutex<Queue>> {
        self.queue.clone()
    }

    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Start the fuzzing campaign with the given amount of workers.
    pub fn start(&mut self, worker_cnt: usize) -> Result<()> {
        info!("Spawning {} worker(s).", worker_cnt);

        let init_shared_barrier = Arc::new(Barrier::new(worker_cnt));

        for _ in 0..worker_cnt {
            let worker = FuzzingWorker::new(
                &self.config,
                self.initialization_done.clone(),
                self.initialization_failed.clone(),
                self.queue.clone(),
                self.shared_virgin_map.clone(),
                self.shared_crash_virgin_map.clone(),
                self.cerebrum.clone(),
                init_shared_barrier.clone(),
            );
            let worker = worker.spawn()?;
            info!("Worker {:?} spawned...", worker.uid());
            self.workers.push(worker);
        }

        Ok(())
    }

    pub fn spawn_additional_worker(&mut self) -> Result<()> {
        let init_shared_barrier = Arc::new(Barrier::new(1));

        let worker = FuzzingWorker::new(
            &self.config,
            self.initialization_done.clone(),
            self.initialization_failed.clone(),
            self.queue.clone(),
            self.shared_virgin_map.clone(),
            self.shared_crash_virgin_map.clone(),
            self.cerebrum.clone(),
            init_shared_barrier.clone(),
        );
        let worker = worker.spawn()?;
        info!("Worker {:?} spawned...", worker.uid());
        self.workers.push(worker);
        Ok(())
    }

    pub fn is_any_worker_alive(&self) -> bool {
        self.workers.iter().any(|worker| worker.is_alive())
    }

    /// Stop the campaign and stop all currently running workers.
    pub fn shutdown(&mut self) -> Result<()> {
        info!("Shutting campaign down...");
        for worker in self.workers.iter_mut() {
            info!("Sending stop signal to worker {:?}", worker.uid());
            worker.request_stop_soon();
        }

        // We send all worker a stop request, lets await their termination.
        for worker in self.workers.iter_mut() {
            let success = worker.join();
            if let Err(e) = success {
                error!("Worker terminated with an error. err={:#?}", e);
                match e.downcast_ref::<String>() {
                    Some(as_string) => {
                        error!("String ({}): {}", as_string.len(), as_string);
                    }
                    None => {
                        error!("Unknown any: {:#?}", e);
                    }
                }
            } else {
                info!("Worker {:?} exited cleanly.", worker.uid())
            }
        }

        let mut global_stats = Vec::new();

        // Print the stats
        for worker in self.workers.iter_mut() {
            let stats = worker.stats();
            let stats_locked = stats.lock().unwrap();
            global_stats.push(stats_locked.clone());
            info!("Stats of worker {:?}: {:#?}", worker, stats_locked);
            info!("execs/s: {:#?}", stats_locked.execs_per_sec());
        }

        let global_execs_s = global_stats
            .iter()
            .map(|e| e.execs_per_sec().unwrap_or(0.0))
            .sum::<f64>();
        let global_avg_execs_s = global_execs_s / self.workers.len() as f64;
        let global_stats_sum = global_stats.iter().cloned().sum::<FuzzerEventCounter>();
        info!("Global stats       : {:#?}", global_stats_sum);
        info!("Total execs/s      : {:.2}", global_execs_s);
        info!("Avg.  execs/s      : {:.2}", global_avg_execs_s);
        info!(
            "Runtime             : {:?}",
            global_stats_sum.init_ts.unwrap().elapsed()
        );

        Ok(())
    }

    /// Dump the campaign state to disk.
    pub fn dump(&self) -> Result<()> {
        let queue = self.queue.lock().unwrap();
        queue.dump(&self.config.general.queue_path())?;
        Ok(())
    }

    pub fn restart_crashed_worker(&mut self) -> Result<()> {
        let mut crashed_workers = Vec::new();

        for worker in self.workers.iter() {
            let worker_uid = worker.uid();
            if !worker.is_alive() && !self.restarted_worker.contains(&worker_uid) {
                log::warn!("Worker {:?} has crashed and will be restarted.", worker_uid);
                crashed_workers.push(worker_uid);
            }
        }

        for worker_uid in crashed_workers.iter() {
            if self.restarted_worker.len() > MAX_WORKER_RESTART_CNT {
                log::error!("Maximum number of worker restarts reached. Aborting.");
                // make sure that we are not getting crash notifications for some worker twice.
                self.restarted_worker.extend(crashed_workers);
                return Ok(());
            }
            self.restarted_worker.push(*worker_uid);
            self.spawn_additional_worker()?;
        }

        Ok(())
    }
}
