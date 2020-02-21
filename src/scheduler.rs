use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::rc::Rc;
use std::sync::Arc;

use crate::dag_scheduler::{CompletionEvent, FetchFailedVals};
use crate::dependency::{Dependency, ShuffleDependencyTrait};
use crate::env;
use crate::error::Result;
use crate::job::JobTracker;
use crate::rdd::{Rdd, RddBase};
use crate::result_task::ResultTask;
use crate::serializable_traits::{Data, SerFunc};
use crate::shuffle::ShuffleMapTask;
use crate::stage::Stage;
use crate::task::{TaskBase, TaskContext, TaskOption};
use futures::stream::StreamExt;
use log::{error, info};
use threadpool::ThreadPool;

pub trait Scheduler {
    fn start(&self);
    fn wait_for_register(&self);
    fn run_job<T: Data, U: Data, F>(
        &self,
        rdd: &dyn Rdd<Item = T>,
        func: F,
        partitions: Vec<i64>,
        allow_local: bool,
    ) -> Vec<U>
    where
        Self: Sized,
        F: Fn(Box<dyn Iterator<Item = T>>) -> U;
    fn stop(&self);
    fn default_parallelism(&self) -> i64;
}

/// Functionality by the library built-in schedulers
pub(crate) trait NativeScheduler {
    /// Fast path for execution. Runs the DD in the driver main thread if possible.
    fn local_execution<T: Data, U: Data, F>(jt: JobTracker<F, U, T>) -> Result<Option<Vec<U>>>
    where
        F: SerFunc((TaskContext, Box<dyn Iterator<Item = T>>)) -> U,
    {
        if jt.final_stage.parents.is_empty() && (jt.num_output_parts == 1) {
            let executor = &mut *env::Env::get().async_rt.lock();
            executor.block_on(async {
                let split = (jt.final_rdd.splits()[jt.output_parts[0]]).clone();
                let task_context = TaskContext::new(jt.final_stage.id, jt.output_parts[0], 0);
                let iter = jt.final_rdd.iterator(split).await?;
                let iter: Vec<T> = iter.lock().into_iter().collect();
                Ok(Some(vec![(&jt.func)((
                    task_context,
                    Box::new(iter.into_iter()),
                ))]))
            })
        } else {
            Ok(None)
        }
    }

    fn new_stage(
        &self,
        rdd_base: Arc<dyn RddBase>,
        shuffle_dependency: Option<Arc<dyn ShuffleDependencyTrait>>,
    ) -> Stage {
        log::debug!("inside new stage");
        env::Env::get()
            .cache_tracker
            .register_rdd(rdd_base.get_rdd_id(), rdd_base.number_of_splits());
        if shuffle_dependency.is_some() {
            log::debug!("shuffle dependency and registering mapoutput tracker");
            self.register_shuffle(
                shuffle_dependency.clone().unwrap().get_shuffle_id(),
                rdd_base.number_of_splits(),
            );
            log::debug!("new stage tracker after");
        }
        let id = self.get_next_stage_id();
        log::debug!("new stage id {}", id);
        let stage = Stage::new(
            id,
            rdd_base.clone(),
            shuffle_dependency,
            self.get_parent_stages(rdd_base),
        );
        self.insert_into_stage_cache(id, stage.clone());
        log::debug!("new stage stage return");
        stage
    }

    fn visit_for_missing_parent_stages(
        &self,
        missing: &mut BTreeSet<Stage>,
        visited: &mut BTreeSet<Arc<dyn RddBase>>,
        rdd: Arc<dyn RddBase>,
    ) {
        log::debug!(
            "missing stages {:?}",
            missing.iter().map(|x| x.id).collect::<Vec<_>>()
        );
        log::debug!(
            "visisted stages {:?}",
            visited.iter().map(|x| x.get_rdd_id()).collect::<Vec<_>>()
        );
        if !visited.contains(&rdd) {
            visited.insert(rdd.clone());
            // TODO CacheTracker register
            for p in 0..rdd.number_of_splits() {
                let locs = self.get_cache_locs(rdd.clone());
                log::debug!("cache locs {:?}", locs);
                if locs == None {
                    for dep in rdd.get_dependencies() {
                        log::debug!("for dep in missing stages ");
                        match dep {
                            Dependency::ShuffleDependency(shuf_dep) => {
                                let stage = self.get_shuffle_map_stage(shuf_dep.clone());
                                log::debug!("shuffle stage in missing stages {:?}", stage.id);
                                if !stage.is_available() {
                                    log::debug!(
                                        "inserting shuffle stage in missing stages {:?}",
                                        stage.id
                                    );
                                    missing.insert(stage);
                                }
                            }
                            Dependency::NarrowDependency(nar_dep) => {
                                log::debug!("narrow stage in missing stages ");
                                self.visit_for_missing_parent_stages(
                                    missing,
                                    visited,
                                    nar_dep.get_rdd_base(),
                                )
                            }
                        }
                    }
                }
            }
        }
    }

    fn visit_for_parent_stages(
        &self,
        parents: &mut BTreeSet<Stage>,
        visited: &mut BTreeSet<Arc<dyn RddBase>>,
        rdd: Arc<dyn RddBase>,
    ) {
        log::debug!(
            "parent stages {:?}",
            parents.iter().map(|x| x.id).collect::<Vec<_>>()
        );
        log::debug!(
            "visisted stages {:?}",
            visited.iter().map(|x| x.get_rdd_id()).collect::<Vec<_>>()
        );
        if !visited.contains(&rdd) {
            visited.insert(rdd.clone());
            env::Env::get()
                .cache_tracker
                .register_rdd(rdd.get_rdd_id(), rdd.number_of_splits());
            for dep in rdd.get_dependencies() {
                match dep {
                    Dependency::ShuffleDependency(shuf_dep) => {
                        parents.insert(self.get_shuffle_map_stage(shuf_dep.clone()));
                    }
                    Dependency::NarrowDependency(nar_dep) => {
                        self.visit_for_parent_stages(parents, visited, nar_dep.get_rdd_base())
                    }
                }
            }
        }
    }

    fn get_parent_stages(&self, rdd: Arc<dyn RddBase>) -> Vec<Stage> {
        log::debug!("inside get parent stages");
        let mut parents: BTreeSet<Stage> = BTreeSet::new();
        let mut visited: BTreeSet<Arc<dyn RddBase>> = BTreeSet::new();
        self.visit_for_parent_stages(&mut parents, &mut visited, rdd.clone());
        log::debug!(
            "parent stages {:?}",
            parents.iter().map(|x| x.id).collect::<Vec<_>>()
        );
        parents.into_iter().collect()
    }

    fn on_event_failure<T: Data, U: Data, F>(
        &self,
        jt: JobTracker<F, U, T>,
        failed_vals: FetchFailedVals,
        stage_id: usize,
    ) where
        F: SerFunc((TaskContext, Box<dyn Iterator<Item = T>>)) -> U,
    {
        let FetchFailedVals {
            server_uri,
            shuffle_id,
            map_id,
            reduce_id,
        } = failed_vals;

        //TODO mapoutput tracker needs to be finished for this
        // let failed_stage = self.id_to_stage.lock().get(&stage_id).unwrap().clone();
        let failed_stage = self.fetch_from_stage_cache(stage_id);
        jt.running.borrow_mut().remove(&failed_stage);
        jt.failed.borrow_mut().insert(failed_stage);
        //TODO logging
        self.remove_output_loc_from_stage(shuffle_id, map_id, &server_uri);
        self.unregister_map_output(shuffle_id, map_id, server_uri);
        //logging
        jt.failed
            .borrow_mut()
            .insert(self.fetch_from_shuffle_to_cache(shuffle_id));
    }

    fn on_event_success<T: Data, U: Data, F>(
        &self,
        mut completed_event: CompletionEvent,
        results: &mut Vec<Option<U>>,
        num_finished: &mut usize,
        jt: JobTracker<F, U, T>,
    ) where
        F: SerFunc((TaskContext, Box<dyn Iterator<Item = T>>)) -> U,
    {
        //TODO logging
        //TODO add to Accumulator

        let mut result_type = completed_event
            .task
            .downcast_ref::<ResultTask<T, U, F>>()
            .is_some();
        if result_type {
            if let Ok(rt) = completed_event.task.downcast::<ResultTask<T, U, F>>() {
                let result = completed_event
                    .result
                    .take()
                    .unwrap()
                    .downcast_ref::<U>()
                    .unwrap()
                    .clone();

                results[rt.output_id] = Some(result);
                jt.finished.borrow_mut()[rt.output_id] = true;
                *num_finished += 1;
            }
        } else if let Ok(smt) = completed_event.task.downcast::<ShuffleMapTask>() {
            let result = completed_event
                .result
                .take()
                .unwrap()
                .downcast_ref::<String>()
                .unwrap()
                .clone();

            log::debug!("result inside queue {:?}", result);
            self.add_output_loc_to_stage(smt.stage_id, smt.partition, result);

            let stage = self.fetch_from_stage_cache(smt.stage_id);
            // id_to_stage.lock().clone()[&smt.stage_id].clone();
            log::debug!(
                "pending stages {:?}",
                jt.pending_tasks
                    .borrow()
                    .iter()
                    .map(|(x, y)| (x.id, y.iter().map(|k| k.get_task_id()).collect::<Vec<_>>()))
                    .collect::<Vec<_>>()
            );
            log::debug!(
                "pending tasks {:?}",
                jt.pending_tasks
                    .borrow()
                    .get(&stage)
                    .unwrap()
                    .iter()
                    .map(|x| x.get_task_id())
                    .collect::<Vec<_>>()
            );
            log::debug!(
                "running {:?}",
                jt.running.borrow().iter().map(|x| x.id).collect::<Vec<_>>()
            );
            log::debug!(
                "waiting {:?}",
                jt.waiting.borrow().iter().map(|x| x.id).collect::<Vec<_>>()
            );

            if jt.running.borrow().contains(&stage)
                && jt.pending_tasks.borrow().get(&stage).unwrap().is_empty()
            {
                log::debug!("here before registering map outputs ");
                //TODO logging
                jt.running.borrow_mut().remove(&stage);
                if stage.shuffle_dependency.is_some() {
                    log::debug!(
                        "stage output locs before register mapoutput tracker {:?}",
                        stage.output_locs
                    );
                    let locs = stage
                        .output_locs
                        .iter()
                        .map(|x| x.get(0).map(|s| s.to_owned()))
                        .collect();
                    log::debug!(
                        "locs for shuffle id {:?} {:?}",
                        stage.clone().shuffle_dependency.unwrap().get_shuffle_id(),
                        locs
                    );
                    self.register_map_outputs(
                        stage.shuffle_dependency.unwrap().get_shuffle_id(),
                        locs,
                    );
                    log::debug!("here after registering map outputs ");
                }
                //TODO Cache
                self.update_cache_locs();
                let mut newly_runnable = Vec::new();
                for stage in jt.waiting.borrow().iter() {
                    log::debug!(
                        "waiting stage parent stages for stage {} are {:?}",
                        stage.id,
                        self.get_missing_parent_stages(stage.clone())
                            .iter()
                            .map(|x| x.id)
                            .collect::<Vec<_>>()
                    );
                    if self.get_missing_parent_stages(stage.clone()).is_empty() {
                        newly_runnable.push(stage.clone())
                    }
                }
                for stage in &newly_runnable {
                    jt.waiting.borrow_mut().remove(stage);
                }
                for stage in &newly_runnable {
                    jt.running.borrow_mut().insert(stage.clone());
                }
                for stage in newly_runnable {
                    self.submit_missing_tasks(stage, jt.clone());
                }
            }
        }
    }

    fn submit_stage<T: Data, U: Data, F>(&self, stage: Stage, jt: JobTracker<F, U, T>)
    where
        F: SerFunc((TaskContext, Box<dyn Iterator<Item = T>>)) -> U,
    {
        log::debug!("submiting stage {}", stage.id);
        if !jt.waiting.borrow().contains(&stage) && !jt.running.borrow().contains(&stage) {
            let missing = self.get_missing_parent_stages(stage.clone());
            log::debug!(
                "inside submit stage missing stages {:?}",
                missing.iter().map(|x| x.id).collect::<Vec<_>>()
            );
            if missing.is_empty() {
                self.submit_missing_tasks(stage.clone(), jt.clone());
                jt.running.borrow_mut().insert(stage);
            } else {
                for parent in missing {
                    self.submit_stage(parent, jt.clone());
                }
                jt.waiting.borrow_mut().insert(stage);
            }
        }
    }

    fn submit_missing_tasks<T: Data, U: Data, F>(&self, stage: Stage, mut jt: JobTracker<F, U, T>)
    where
        F: SerFunc((TaskContext, Box<dyn Iterator<Item = T>>)) -> U,
    {
        let mut pending_tasks = jt.pending_tasks.borrow_mut();
        let my_pending = pending_tasks
            .entry(stage.clone())
            .or_insert_with(BTreeSet::new);
        if stage == jt.final_stage {
            log::debug!("final stage {}", stage.id);
            for (id_in_job, (id, part)) in jt
                .output_parts
                .iter()
                .enumerate()
                .take(jt.num_output_parts)
                .enumerate()
            {
                let locs = self.get_preferred_locs(jt.final_rdd.get_rdd_base(), *part);
                let result_task = ResultTask::new(
                    self.get_next_task_id(),
                    jt.run_id,
                    jt.final_stage.id,
                    jt.final_rdd.clone(),
                    jt.func.clone(),
                    *part,
                    locs,
                    id,
                );
                let task = Box::new(result_task.clone()) as Box<dyn TaskBase>;
                let executor = self.next_executor_server(&*task);
                my_pending.insert(task);
                self.submit_task::<T, U, F>(
                    TaskOption::ResultTask(Box::new(result_task)),
                    id_in_job,
                    jt.thread_pool.clone(),
                    executor,
                );
            }
        } else {
            for p in 0..stage.num_partitions {
                log::debug!("shuffle_stage {}", stage.id);
                if stage.output_locs[p].is_empty() {
                    let locs = self.get_preferred_locs(stage.get_rdd(), p);
                    log::debug!("creating task for {} partition  {}", stage.id, p);
                    let shuffle_map_task = ShuffleMapTask::new(
                        self.get_next_task_id(),
                        jt.run_id,
                        stage.id,
                        stage.rdd.clone(),
                        stage.shuffle_dependency.clone().unwrap(),
                        p,
                        locs,
                    );
                    log::debug!(
                        "creating task for {} partition  {} and shuffle id {}",
                        stage.id,
                        p,
                        shuffle_map_task.dep.get_shuffle_id()
                    );
                    let task = Box::new(shuffle_map_task.clone()) as Box<dyn TaskBase>;
                    let executor = self.next_executor_server(&*task);
                    my_pending.insert(task);
                    self.submit_task::<T, U, F>(
                        TaskOption::ShuffleMapTask(Box::new(shuffle_map_task)),
                        p,
                        jt.thread_pool.clone(),
                        executor,
                    );
                }
            }
        }
    }

    fn submit_task<T: Data, U: Data, F>(
        &self,
        task: TaskOption,
        id_in_job: usize,
        thread_pool: Rc<ThreadPool>,
        target_executor: SocketAddrV4,
    ) where
        F: SerFunc((TaskContext, Box<dyn Iterator<Item = T>>)) -> U;

    // mutators:
    fn add_output_loc_to_stage(&self, stage_id: usize, partition: usize, host: String);
    fn insert_into_stage_cache(&self, id: usize, stage: Stage);
    /// refreshes cache locations
    fn register_shuffle(&self, shuffle_id: usize, num_maps: usize);
    fn register_map_outputs(&self, shuffle_id: usize, locs: Vec<Option<String>>);
    fn remove_output_loc_from_stage(&self, shuffle_id: usize, map_id: usize, server_uri: &str);
    fn update_cache_locs(&self);
    fn unregister_map_output(&self, shuffle_id: usize, map_id: usize, server_uri: String);

    // getters:
    fn fetch_from_stage_cache(&self, id: usize) -> Stage;
    fn fetch_from_shuffle_to_cache(&self, id: usize) -> Stage;
    fn get_cache_locs(&self, rdd: Arc<dyn RddBase>) -> Option<Vec<Vec<Ipv4Addr>>>;
    fn get_missing_parent_stages(&self, stage: Stage) -> Vec<Stage>;
    fn get_next_job_id(&self) -> usize;
    fn get_next_stage_id(&self) -> usize;
    fn get_next_task_id(&self) -> usize;
    fn next_executor_server(&self, rdd: &dyn TaskBase) -> SocketAddrV4;

    fn get_preferred_locs(&self, rdd: Arc<dyn RddBase>, partition: usize) -> Vec<Ipv4Addr> {
        //TODO have to implement this completely
        if let Some(cached) = self.get_cache_locs(rdd.clone()) {
            if let Some(cached) = cached.get(partition) {
                return cached.clone();
            }
        }
        let rdd_prefs = rdd.preferred_locations(rdd.splits()[partition].clone());
        if !rdd.is_pinned() {
            if !rdd_prefs.is_empty() {
                return rdd_prefs;
            }
            for dep in rdd.get_dependencies().iter() {
                if let Dependency::NarrowDependency(nar_dep) = dep {
                    for in_part in nar_dep.get_parents(partition) {
                        let locs = self.get_preferred_locs(nar_dep.get_rdd_base(), in_part);
                        if !locs.is_empty() {
                            return locs;
                        }
                    }
                }
            }
            Vec::new()
        } else {
            // when pinned, is required that there is exactly one preferred location
            // for a given partition
            assert!(rdd_prefs.len() == 1);
            rdd_prefs
        }
    }

    fn get_shuffle_map_stage(&self, shuf: Arc<dyn ShuffleDependencyTrait>) -> Stage;

    fn num_threads(&self) -> usize {
        num_cpus::get()
    }
}

macro_rules! impl_common_scheduler_funcs {
    () => {
        fn add_output_loc_to_stage(&self, stage_id: usize, partition: usize, host: String) {
            self.stage_cache
                .lock()
                .get_mut(&stage_id)
                .unwrap()
                .add_output_loc(partition, host);
        }

        fn insert_into_stage_cache(&self, id: usize, stage: Stage) {
            self.stage_cache.lock().insert(id, stage.clone());
        }

        fn fetch_from_stage_cache(&self, id: usize) -> Stage {
            self.stage_cache.lock().get(&id).unwrap().clone()
        }

        fn fetch_from_shuffle_to_cache(&self, id: usize) -> Stage {
            self.shuffle_to_map_stage.lock().get(&id).unwrap().clone()
        }

        fn update_cache_locs(&self) {
            let mut locs = self.cache_locs.lock();
            *locs = env::Env::get().cache_tracker.get_location_snapshot();
        }

        fn unregister_map_output(&self, shuffle_id: usize, map_id: usize, server_uri: String) {
            self.map_output_tracker.unregister_map_output(
                shuffle_id,
                map_id,
                server_uri
            )
        }

        fn register_shuffle(&self, shuffle_id: usize, num_maps: usize) {
            self.map_output_tracker.register_shuffle(
                shuffle_id,
                num_maps
            )
        }

        fn register_map_outputs(&self, shuffle_id: usize, locs: Vec<Option<String>>) {
            self.map_output_tracker.register_map_outputs(
                shuffle_id,
                locs
            )
        }

        fn remove_output_loc_from_stage(&self, shuffle_id: usize, map_id: usize, server_uri: &str) {
            self.shuffle_to_map_stage
                .lock()
                .get_mut(&shuffle_id)
                .unwrap()
                .remove_output_loc(map_id, server_uri);
        }

        fn get_cache_locs(&self, rdd: Arc<dyn RddBase>) -> Option<Vec<Vec<Ipv4Addr>>> {
            let cache_locs = self.cache_locs.lock();
            let locs_opt = cache_locs.get(&rdd.get_rdd_id());
            locs_opt.cloned()
        }

        fn get_next_job_id(&self) -> usize {
            self.next_job_id.fetch_add(1, Ordering::SeqCst)
        }

        fn get_next_stage_id(&self) -> usize {
            self.next_stage_id.fetch_add(1, Ordering::SeqCst)
        }

        fn get_next_task_id(&self) -> usize {
            self.next_task_id.fetch_add(1, Ordering::SeqCst)
        }

        fn get_missing_parent_stages(&self, stage: Stage) -> Vec<Stage> {
            log::debug!("inside get missing parent stages");
            let mut missing: BTreeSet<Stage> = BTreeSet::new();
            let mut visited: BTreeSet<Arc<dyn RddBase>> = BTreeSet::new();
            self.visit_for_missing_parent_stages(&mut missing, &mut visited, stage.get_rdd());
            missing.into_iter().collect()
        }

        fn get_shuffle_map_stage(&self, shuf: Arc<dyn ShuffleDependencyTrait>) -> Stage {
            log::debug!("inside get_shufflemap stage");
            let stage = self.shuffle_to_map_stage.lock().get(&shuf.get_shuffle_id()).cloned();
            match stage {
                Some(stage) => stage.clone(),
                None => {
                    log::debug!("inside get_shufflemap stage before");
                    let stage = self.new_stage(shuf.get_rdd_base(), Some(shuf.clone()));
                    self.shuffle_to_map_stage
                        .lock()
                        .insert(shuf.get_shuffle_id(), stage.clone());
                    log::debug!("inside get_shufflemap return");
                    stage
                }
            }
        }

        fn num_threads(&self) -> usize {
            self.threads
        }
    };
}
