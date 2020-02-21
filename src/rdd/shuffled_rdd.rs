use std::collections::HashMap;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use crate::aggregator::Aggregator;
use crate::context::Context;
use crate::dependency::{Dependency, ShuffleDependency};
use crate::error::{Error, Result};
use crate::partitioner::Partitioner;
use crate::rdd::{AnyDataStream, ComputeResult, Rdd, RddBase, RddVals};
use crate::serializable_traits::{AnyData, Data};
use crate::shuffle::ShuffleFetcher;
use crate::split::Split;
use futures::{FutureExt, Stream, StreamExt};
use log::info;
use parking_lot::Mutex;
use serde_derive::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
struct ShuffledRddSplit {
    index: usize,
}

impl ShuffledRddSplit {
    fn new(index: usize) -> Self {
        ShuffledRddSplit { index }
    }
}

impl Split for ShuffledRddSplit {
    fn get_index(&self) -> usize {
        self.index
    }
}

#[derive(Serialize, Deserialize)]
pub struct ShuffledRdd<K: Data + Eq + Hash, V: Data, C: Data> {
    #[serde(with = "serde_traitobject")]
    parent: Arc<dyn Rdd<Item = (K, V)>>,
    #[serde(with = "serde_traitobject")]
    aggregator: Arc<Aggregator<K, V, C>>,
    vals: Arc<RddVals>,
    #[serde(with = "serde_traitobject")]
    part: Box<dyn Partitioner>,
    shuffle_id: usize,
}

impl<K: Data + Eq + Hash, V: Data, C: Data> Clone for ShuffledRdd<K, V, C> {
    fn clone(&self) -> Self {
        ShuffledRdd {
            parent: self.parent.clone(),
            aggregator: self.aggregator.clone(),
            vals: self.vals.clone(),
            part: self.part.clone(),
            shuffle_id: self.shuffle_id,
        }
    }
}

impl<K: Data + Eq + Hash, V: Data, C: Data> ShuffledRdd<K, V, C> {
    pub(crate) fn new(
        parent: Arc<dyn Rdd<Item = (K, V)>>,
        aggregator: Arc<Aggregator<K, V, C>>,
        part: Box<dyn Partitioner>,
    ) -> Self {
        let mut vals = RddVals::new(parent.get_context());
        let shuffle_id = vals.context.new_shuffle_id();

        vals.dependencies
            .push(Dependency::ShuffleDependency(Arc::new(
                ShuffleDependency::new(
                    shuffle_id,
                    false,
                    parent.get_rdd_base(),
                    aggregator.clone(),
                    part.clone(),
                ),
            )));
        let vals = Arc::new(vals);
        ShuffledRdd {
            parent,
            aggregator,
            vals,
            part,
            shuffle_id,
        }
    }
}

#[async_trait::async_trait]
impl<K: Data + Eq + Hash, V: Data, C: Data> RddBase for ShuffledRdd<K, V, C> {
    fn get_rdd_id(&self) -> usize {
        self.vals.id
    }

    fn get_context(&self) -> Arc<Context> {
        self.vals.context.clone()
    }

    fn get_dependencies(&self) -> Vec<Dependency> {
        self.vals.dependencies.clone()
    }

    fn splits(&self) -> Vec<Box<dyn Split>> {
        (0..self.part.get_num_of_partitions())
            .map(|x| Box::new(ShuffledRddSplit::new(x)) as Box<dyn Split>)
            .collect()
    }

    fn number_of_splits(&self) -> usize {
        self.part.get_num_of_partitions()
    }

    fn partitioner(&self) -> Option<Box<dyn Partitioner>> {
        Some(self.part.clone())
    }

    async fn iterator_any(&self, split: Box<dyn Split>) -> Result<AnyDataStream> {
        log::debug!("inside iterator_any shuffledrdd",);
        super::iterator_any_tuple(self, split).await
    }

    async fn cogroup_iterator_any(&self, split: Box<dyn Split>) -> Result<AnyDataStream> {
        log::debug!("inside cogroup iterator_any shuffledrdd",);
        super::cogroup_iterator_any(self, split).await
    }
}

#[async_trait::async_trait]
impl<K: Data + Eq + Hash, V: Data, C: Data> Rdd for ShuffledRdd<K, V, C> {
    type Item = (K, C);

    fn get_rdd_base(&self) -> Arc<dyn RddBase> {
        Arc::new(self.clone()) as Arc<dyn RddBase>
    }

    fn get_rdd(&self) -> Arc<dyn Rdd<Item = Self::Item>> {
        Arc::new(self.clone())
    }

    async fn compute(&self, split: Box<dyn Split>) -> Result<ComputeResult<Self::Item>> {
        log::debug!("compute inside shuffled rdd");
        let mut combiners: HashMap<K, Option<C>> = HashMap::new();
        let merge_pair = |(k, c): (K, C)| {
            if let Some(old_c) = combiners.get_mut(&k) {
                let old = old_c.take().unwrap();
                let input = ((old, c),);
                let output = self.aggregator.merge_combiners.call(input);
                *old_c = Some(output);
            } else {
                combiners.insert(k, Some(c));
            }
        };

        let start = Instant::now();
        ShuffleFetcher::fetch(self.shuffle_id, split.get_index(), merge_pair).await;
        log::debug!("time taken for fetching {}", start.elapsed().as_millis());
        Ok(Arc::new(Mutex::new(
            combiners.into_iter().map(|(k, v)| (k, v.unwrap())),
        )))
    }
}
