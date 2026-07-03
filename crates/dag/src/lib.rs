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

    pub struct DagDependentsIterator<'a, D: DagNode> {
        inner: &'a Vec<DagItem<D>>,
        seen: Vec<bool>,
        idxes: Vec<usize>,
    }

    impl<'a, D: DagNode> Iterator for DagDependentsIterator<'a, D> {
        type Item = Vec<&'a D>;
        fn next(&mut self) -> Option<Self::Item> {
            let next_idxes = self
                .idxes
                .iter()
                .flat_map(|idx_before| {
                    let idx_next = &self.inner[*idx_before].dependents_indexes;
                    let mut res = Vec::new();
                    for idx_next in idx_next {
                        let seen = &mut self.seen[*idx_next];
                        if !*seen {
                            *seen = true;
                            res.push(*idx_next);
                        }
                    }
                    res
                })
                .collect();
            let idxes = std::mem::replace(&mut self.idxes, next_idxes);
            if idxes.is_empty() {
                None
            } else {
                Some(idxes.into_iter().map(|i| &self.inner[i].inner).collect())
            }
        }
    }

    /// Arguments of the function which is used to map DagIterator
    pub struct DagIteratorMapFuncArgs<'a, D: DagNode> {
        /// Item itself
        pub inner: D,
        /// Index of the node within the original input order
        pub index: usize,
        /// Longest dependency chain depth (0 if no dependencies)
        pub depth: usize,
        /// References to dependents items
        pub dependents_iter: DagDependentsIterator<'a, D>,
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
                let dependents_iter = DagDependentsIterator {
                    inner,
                    seen: vec![false; inner.len()],
                    idxes: item.dependents_indexes,
                };
                map_func(DagIteratorMapFuncArgs {
                    inner: item.inner,
                    index: item.original_index,
                    depth: item.depth,
                    dependents_iter,
                })
            })
        }
    }
}

/// Dag Node Trait
pub trait DagNode {
    /// 名前。`None` のノードは重複チェック・被依存・cycle の対象外となり、
    /// ID を持たない「末端」ノードとして扱われる。
    fn id(&self) -> Option<&str>;
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>>;
}

/// Dag Resolution Error
#[derive(Debug, Error)]
pub enum DagError {
    #[error("duplicate node: {0}")]
    DuplicateName(String),
    #[error(
        "unknown dependency: {dep} (referred by {by})",
        by = by.as_deref().unwrap_or("<unnamed>")
    )]
    UnknownDependency { dep: String, by: Option<String> },
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
        /// Index within the original input order
        pub(super) original_index: usize,
        /// Longest dependency chain depth (0 if no dependencies)
        pub(super) depth: usize,
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
            .enumerate()
            .map(|(original_index, node)| DagItem {
                inner: node,
                original_index,
                depth: 0,
                dependents_indexes: Vec::new(),
            })
            .collect();

        let n = nodes.len();

        let mut waiting = Vec::with_capacity(n);
        let mut references: Vec<Vec<usize>> = vec![Vec::new(); n];
        {
            // 2) &str をキーにした id → index マップを作成（ここで重複検出）。
            //    名前なしノード（id() == None）は登録せず、被依存にもならない。
            let mut id_to_index: HashMap<&str, usize> = HashMap::with_capacity(n);
            for (i, dag) in nodes.iter().enumerate() {
                if let Some(id) = dag.inner.id()
                    && id_to_index.insert(id, i).is_some()
                {
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
                                by: item.inner.id().map(str::to_string),
                            })?;
                    references[dep_idx].push(idx);
                }
                waiting.push(deps.len());
            }
        }

        // 4) Kahn法でトポロジカルソート（ついでに依存の最深 depth を計算）。
        //    i を処理する時点で nodes[i].depth は確定済み（依存先は全て先に処理される）ので、
        //    1パスで正しい最深が入る。
        let mut q = Vec::new();
        for (i, &deg) in waiting.iter().enumerate() {
            if deg == 0 {
                q.push(i);
            }
        }
        let mut topo = VecDeque::with_capacity(n); // VecDequeなのは先入れ先出しで取り出すため
        while let Some(i) = q.pop() {
            topo.push_front(i);
            let depth_i = nodes[i].depth;
            for &to in &references[i] {
                // depth[to] = max(depth[to], depth[i] + 1)
                let next = depth_i + 1;
                if next > nodes[to].depth {
                    nodes[to].depth = next;
                }
                waiting[to] -= 1;
                if waiting[to] == 0 {
                    q.push(to);
                }
            }
        }
        drop(q);

        if topo.len() != n {
            // 残っているノードはサイクルの一部。
            // 名前なしノードは被依存になれないためサイクルに含まれず、
            // id() == Some のノードのみ文字列化する。
            return Err(DagError::CycleDetected(
                waiting
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, deg)| {
                        if deg > 0 {
                            nodes[i].inner.id().map(str::to_string)
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
        let mut topo_positions = vec![0usize; n];
        for (topo_index, &node_index) in topo.iter().enumerate() {
            topo_positions[node_index] = topo_index;
        }
        for (node, r) in nodes.iter_mut().zip(references) {
            node.dependents_indexes = r.into_iter().map(|e| topo_positions[e]).collect();
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
