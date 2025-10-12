#[cfg(feature = "hashbrown")]
use hashbrown::HashMap;
#[cfg(not(feature = "hashbrown"))]
use std::collections::HashMap;

use std::collections::VecDeque;
use thiserror::Error;

use {
    iterator::{DagIterator, DagIteratorMapFuncArgs},
    tree::{DagItem, DagTree},
};

pub mod iterator {
    use super::*;

    /// Arguments of the function which is used to map DagIterator
    pub struct DagIteratorMapFuncArgs<'a, D: DagNode> {
        /// Item itself
        pub inner: D,
        /// References to dependents items
        pub dependents: Vec<&'a D>,
    }

    /// Dag Iterator with mapping function
    pub struct DagIterator<T, D: DagNode, F: FnMut(DagIteratorMapFuncArgs<D>) -> T> {
        pub(super) inner: Vec<DagItem<D>>,
        pub(super) map_func: F,
    }

    impl<T, D: DagNode, F: FnMut(DagIteratorMapFuncArgs<D>) -> T> Iterator for DagIterator<T, D, F> {
        type Item = T;

        fn next(&mut self) -> Option<Self::Item> {
            let Self { inner, map_func } = self;

            inner.pop().map(|item| {
                let dependents = item
                    .dependents_indexes
                    .iter()
                    .map(|&i| &inner[i].inner)
                    .collect();
                map_func(DagIteratorMapFuncArgs {
                    inner: item.inner,
                    dependents,
                })
            })
        }
    }
}

/// Dag Node Trait
pub trait DagNode {
    fn id(&self) -> &str;
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>>;
}

/// Dag Resolution Error
#[derive(Debug, Error)]
pub enum DagError {
    #[error("duplicate node: {0}")]
    DuplicateName(String),
    #[error("unknown dependency: {dep} (referred by {by})")]
    UnknownDependency { dep: String, by: String },
    #[error("cycle detected; remaining: {0:?}")]
    CycleDetected(Vec<String>),
}

pub mod tree {
    use super::*;

    /// Resolved DAG Tree
    pub struct DagTree<D: DagNode> {
        pub(super) inner: Vec<DagItem<D>>,
    }

    pub(super) struct DagItem<D: DagNode> {
        pub(super) inner: D,
        pub(super) dependents_indexes: Vec<usize>,
    }
}

/// Extension to IntoIterator<D>: Allow DAG resolution to be called by the method
pub trait TryDag<D: DagNode>: IntoIterator<Item = D> + Sized {
    /// Consume self to resolve the DAG and return a topo-ordered DagTree
    fn try_dag(self) -> Result<DagTree<D>, DagError> {
        // 1) まず全ノードを集める（この段階では重複チェックしない）
        let mut nodes: Vec<DagItem<D>> = self
            .into_iter()
            .map(|node| DagItem {
                inner: node,
                dependents_indexes: Vec::new(),
            })
            .collect();

        let n = nodes.len();

        let mut waiting = Vec::with_capacity(n);
        let mut references: Vec<Vec<usize>> = vec![Vec::new(); n];
        {
            // 2) &str をキーにした id → index マップを作成（ここで重複検出）
            let mut id_to_index: HashMap<&str, usize> = HashMap::with_capacity(n);
            for (i, dag) in nodes.iter().enumerate() {
                let id = dag.inner.id(); // &str
                if id_to_index.insert(id, i).is_some() {
                    // ここだけエラーメッセージ用に to_string()
                    return Err(DagError::DuplicateName(id.to_string()));
                }
            }

            // 3) 依存グラフ（Kahn法用）と dependents の一時格納
            for (idx, item) in nodes.iter().enumerate() {
                let deps: Vec<_> = item.inner.depends().into_iter().collect();
                for dep in &deps {
                    let dep = dep.as_ref();
                    let &dep_idx =
                        id_to_index
                            .get(dep)
                            .ok_or_else(|| DagError::UnknownDependency {
                                dep: dep.to_string(),
                                by: item.inner.id().to_string(),
                            })?;
                    references[dep_idx].push(idx);
                }
                waiting.push(deps.len());
            }
        }

        // 4) Kahn法でトポロジカルソート
        let mut q = Vec::new();
        for (i, &deg) in waiting.iter().enumerate() {
            if deg == 0 {
                q.push(i);
            }
        }
        let mut topo = VecDeque::with_capacity(n); // VecDequeなのは先入れ先出しで取り出すため
        while let Some(i) = q.pop() {
            topo.push_front(i);
            for &to in &references[i] {
                waiting[to] -= 1;
                if waiting[to] == 0 {
                    q.push(to);
                }
            }
        }
        drop(q);

        if topo.len() != n {
            // 残っているノードはサイクルの一部
            return Err(DagError::CycleDetected(
                waiting
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, deg)| {
                        if deg > 0 {
                            Some(nodes[i].inner.id().to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
            ));
        } else {
            drop(waiting);
        }

        // 7) 現時点での依存関係をセット
        for (node, r) in nodes.iter_mut().zip(references) {
            node.dependents_indexes = r
                .into_iter()
                .map(|e| {
                    topo.iter()
                        .enumerate()
                        .find_map(|(i, t)| if *t == e { Some(i) } else { None })
                        .unwrap()
                })
                .collect();
        }

        // 6) topo 順に取り出す（swap_removeは使わずtake）
        let inner: Vec<DagItem<D>> = {
            let mut nodes_opt: Vec<_> = nodes.into_iter().map(Some).collect();
            topo.into_iter()
                .map(|i| nodes_opt[i].take().unwrap())
                .collect()
        };

        Ok(DagTree { inner })
    }
}

// Automatic implementation for all IntoIterator<Item = D>.
impl<D: DagNode, I: IntoIterator<Item = D>> TryDag<D> for I {}

impl<D: DagNode> DagTree<D> {
    /// Iterate with mapping
    pub fn into_map_iter<T, F: FnMut(DagIteratorMapFuncArgs<D>) -> T>(
        self,
        map_func: F,
    ) -> DagIterator<T, D, F> {
        DagIterator {
            inner: self.inner,
            map_func,
        }
    }
}

impl<D: DagNode> IntoIterator for DagTree<D> {
    type Item = D;

    type IntoIter = DagIterator<D, D, fn(DagIteratorMapFuncArgs<D>) -> D>;

    fn into_iter(self) -> Self::IntoIter {
        fn unwrap<D: DagNode>(d: DagIteratorMapFuncArgs<D>) -> D {
            d.inner
        }
        self.into_map_iter(unwrap)
    }
}
