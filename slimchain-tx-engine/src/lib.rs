#[macro_use]
extern crate tracing;

use crossbeam::{
    deque::{Injector, Stealer, Worker},
    queue::{ArrayQueue, SegQueue},
    sync::{Parker, Unparker},
    utils::Backoff,
};
use slimchain_common::{
    basic::{BlockHeight, H256},
    create_id_type_u32,
    error::Result,
    tx::TxTrait,
    tx_req::SignedTxRequest,
};
use slimchain_tx_state::{TxProposal, TxStateView, TxWriteSetTrie};
use slimchain_utils::record_time;
use std::{
    iter,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

create_id_type_u32!(TxTaskId);

pub trait TxEngineWorker: Send {
    type Output: TxTrait;

    fn execute(&self, task: TxTask) -> Result<Self::Output>;
}

pub struct TxTask {
    pub id: TxTaskId,
    pub block_height: BlockHeight,
    pub state_view: Arc<dyn TxStateView + Sync + Send>,
    pub state_root: H256,
    pub signed_tx_req: SignedTxRequest,
}

impl TxTask {
    pub fn new(
        block_height: BlockHeight,
        state_view: Arc<dyn TxStateView + Sync + Send>,
        state_root: H256,
        signed_tx_req: SignedTxRequest,
    ) -> Self {
        let id = TxTaskId::next_id();

        Self {
            id,
            block_height,
            state_view,
            state_root,
            signed_tx_req,
        }
    }

    pub fn get_id(&self) -> TxTaskId {
        self.id
    }
}

pub struct TxTaskOutput<Tx: TxTrait> {
    pub task_id: TxTaskId,
    pub tx_proposal: TxProposal<Tx>,
}

pub struct TxEngine<Tx: TxTrait + 'static> {
    task_queue: Arc<Injector<TxTask>>,
    result_queue: Arc<SegQueue<TxTaskOutput<Tx>>>,
    unparker_queue: Arc<ArrayQueue<Unparker>>,
    shutdown_flag: Arc<AtomicBool>,
    worker_threads: Vec<JoinHandle<()>>,
    remaining_tasks: Arc<AtomicUsize>,
}

impl<Tx: TxTrait + 'static> TxEngine<Tx> {
    #[tracing::instrument(name = "tx_engine_init", skip(threads, worker_factory))]
    pub fn new(
        threads: usize,
        worker_factory: impl Fn() -> Box<dyn TxEngineWorker<Output = Tx>>,
    ) -> Self {
        info!("Spawning TxEngine workers in {} threads.", threads);

        let task_queue = Arc::new(Injector::new());
        let result_queue = Arc::new(SegQueue::new());
        let unparker_queue = Arc::new(ArrayQueue::new(threads));
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let remaining_tasks = Arc::new(AtomicUsize::new(0));

        let mut workers: Vec<_> = (0..threads)
            .map(|_| {
                TxEngineWorkerInstance::new(
                    worker_factory(),
                    task_queue.clone(),
                    threads - 1,
                    result_queue.clone(),
                    unparker_queue.clone(),
                    shutdown_flag.clone(),
                    remaining_tasks.clone(),
                )
            })
            .collect();

        let stealers: Vec<_> = workers.iter().map(|w| w.get_local_stealer()).collect();

        for (i, worker) in workers.iter_mut().enumerate() {
            for (j, stealer) in stealers.iter().enumerate() {
                if i != j {
                    worker.add_global_stealer(stealer.clone());
                }
            }
        }

        let worker_threads: Vec<_> = workers
            .into_iter()
            .map(|w| thread::spawn(move || w.run()))
            .collect();

        Self {
            task_queue,
            result_queue,
            unparker_queue,
            shutdown_flag,
            worker_threads,
            remaining_tasks,
        }
    }

    pub fn remaining_tasks(&self) -> usize {
        self.remaining_tasks.load(Ordering::SeqCst)
    }

    pub fn push_task(&self, task: TxTask) {
        self.remaining_tasks.fetch_add(1, Ordering::SeqCst);
        self.task_queue.push(task);
        if let Ok(unparker) = self.unparker_queue.pop() {
            unparker.unpark();
        }
    }

    pub fn pop_result(&self) -> Option<TxTaskOutput<Tx>> {
        let result = self.result_queue.pop().ok();
        if result.is_some() {
            self.remaining_tasks.fetch_sub(1, Ordering::SeqCst);
        }
        result
    }

    pub fn pop_or_wait_result(&self) -> TxTaskOutput<Tx> {
        let backoff = Backoff::new();
        loop {
            if let Some(output) = self.pop_result() {
                return output;
            }

            backoff.snooze();
        }
    }
}

impl<Tx: TxTrait + 'static> Drop for TxEngine<Tx> {
    #[tracing::instrument(name = "tx_engine_drop", skip(self))]
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);

        thread::sleep(Duration::from_millis(10));
        while let Ok(unpacker) = self.unparker_queue.pop() {
            unpacker.unpark();
        }

        info!("Waiting TxEngine workers to be shutdown.");
        for w in self.worker_threads.drain(..) {
            w.join()
                .expect("TxEngine: Failed to join the worker thread.");
        }
        info!("TxEngine is shutdown.");
    }
}

struct TxEngineWorkerInstance<Tx: TxTrait> {
    global_task_queue: Arc<Injector<TxTask>>,
    local_task_queue: Worker<TxTask>,
    stealers: Vec<Stealer<TxTask>>,
    result_queue: Arc<SegQueue<TxTaskOutput<Tx>>>,
    unparker_queue: Arc<ArrayQueue<Unparker>>,
    shutdown_flag: Arc<AtomicBool>,
    remaining_tasks: Arc<AtomicUsize>,
    worker: Box<dyn TxEngineWorker<Output = Tx>>,
}

impl<Tx: TxTrait> TxEngineWorkerInstance<Tx> {
    fn new(
        worker: Box<dyn TxEngineWorker<Output = Tx>>,
        global_task_queue: Arc<Injector<TxTask>>,
        stealer_num: usize,
        result_queue: Arc<SegQueue<TxTaskOutput<Tx>>>,
        unparker_queue: Arc<ArrayQueue<Unparker>>,
        shutdown_flag: Arc<AtomicBool>,
        remaining_tasks: Arc<AtomicUsize>,
    ) -> Self {
        let local_task_queue = Worker::new_fifo();

        Self {
            global_task_queue,
            local_task_queue,
            stealers: Vec::with_capacity(stealer_num),
            result_queue,
            unparker_queue,
            shutdown_flag,
            remaining_tasks,
            worker,
        }
    }

    fn get_local_stealer(&self) -> Stealer<TxTask> {
        self.local_task_queue.stealer()
    }

    fn add_global_stealer(&mut self, stealer: Stealer<TxTask>) {
        self.stealers.push(stealer);
    }

    fn find_task(&self) -> Option<TxTask> {
        self.local_task_queue.pop().or_else(|| {
            iter::repeat_with(|| {
                self.global_task_queue
                    .steal_batch_and_pop(&self.local_task_queue)
                    .or_else(|| self.stealers.iter().map(|s| s.steal()).collect())
            })
            .find(|s| !s.is_retry())
            .and_then(|s| s.success())
        })
    }

    fn wait_until_task(&self) -> Option<TxTask> {
        let backoff = Backoff::new();
        loop {
            match self.find_task() {
                Some(task) => return Some(task),
                None => {
                    if backoff.is_completed() {
                        if self.shutdown_flag.load(Ordering::Acquire) {
                            return None;
                        }

                        let parker = Parker::new();
                        self.unparker_queue
                            .push(parker.unparker().clone())
                            .expect("TxEngine: Failed to send unparker.");
                        parker.park();
                    } else {
                        backoff.snooze();
                    }
                }
            }
        }
    }

    fn run(&self) {
        while let Some(task) = self.wait_until_task() {
            let span = debug_span!("execute_task", id = task.id.0);
            let _enter = span.enter();

            let begin = Instant::now();
            let task_id = task.get_id();
            let state_view = task.state_view.clone();
            let root_address = task.state_root;
            let tx = match self.worker.execute(task) {
                Ok(output) => output,
                Err(e) => {
                    error!("Failed to execute task. Error: {}", e);
                    self.remaining_tasks.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
            };
            let write_trie = match TxWriteSetTrie::new(&state_view, root_address, tx.tx_writes()) {
                Ok(trie) => trie,
                Err(e) => {
                    error!("Failed to create TxWriteSetTrie. Error: {}", e);
                    self.remaining_tasks.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
            };
            record_time!("exec_time", Instant::now() - begin, "task_id": task_id.0, "tx_id": tx.id());
            self.result_queue.push(TxTaskOutput {
                task_id,
                tx_proposal: TxProposal::new(tx, write_trie),
            });
        }
    }
}
