//! Pure merge planning for loaded plugins.
//!
//! Ordering, compatibility probing, and fixed-point group construction live
//! here so publication/copy code only consumes a deterministic plan.

use super::*;

pub(super) struct MergePlanner;

impl MergePlanner {
    pub(super) fn plan(plugs: &mut BinaryHeap<LoadedPlugin>) {
        let mut items = Vec::with_capacity(plugs.len());
        while let Some(plug) = plugs.pop() {
            items.push(MergeEntry {
                key: MergeSortKey::new(&plug),
                plugin: plug,
            });
        }
        items.sort_by(|left, right| left.key.cmp(&right.key));

        let mut groups: Vec<Option<MergeEntry>> = Vec::with_capacity(items.len());
        let mut ordered = BTreeSet::<(MergeSortKey, usize)>::new();
        for item in items {
            let mut pending = Some(item);
            loop {
                let mut merged = false;
                let candidates = ordered.iter().map(|(_, index)| *index).collect::<Vec<_>>();
                for i in candidates {
                    crate::rsplug::perf::incr(crate::rsplug::perf::PerfOp::MergeAttempt);
                    let Some(candidate) = groups[i].take() else {
                        continue;
                    };
                    ordered.remove(&(candidate.key.clone(), i));
                    let current = pending
                        .take()
                        .expect("merge candidate loop must retain a pending plugin");
                    let pending_key = current.key;
                    let pending_plugin = current.plugin;
                    match candidate.plugin + pending_plugin {
                        (merged_group, None) => {
                            pending = Some(MergeEntry {
                                key: MergeSortKey::new(&merged_group),
                                plugin: merged_group,
                            });
                            merged = true;
                            break;
                        }
                        (candidate_plugin, Some(rest)) => {
                            let entry = MergeEntry {
                                key: candidate.key,
                                plugin: candidate_plugin,
                            };
                            ordered.insert((entry.key.clone(), i));
                            groups[i] = Some(entry);
                            pending = Some(MergeEntry {
                                key: pending_key,
                                plugin: rest,
                            });
                        }
                    }
                }
                if !merged {
                    break;
                }
            }
            let pending = pending.take().expect("merge retains an unmerged plugin");
            let index = groups.len();
            ordered.insert((pending.key.clone(), index));
            groups.push(Some(pending));
        }
        plugs.extend(
            ordered
                .into_iter()
                .filter_map(|(_, index)| groups[index].take())
                .map(|entry| entry.plugin),
        );
    }
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct MergeSortKey {
    lazy_type: LazyType,
    order: usize,
    is_lazy_registration: bool,
    plugin_id: PluginID,
}

impl MergeSortKey {
    fn new(plugin: &LoadedPlugin) -> Self {
        Self {
            lazy_type: plugin.lazy_type.clone(),
            order: plugin.order,
            is_lazy_registration: plugin.is_lazy_registration,
            plugin_id: plugin.plugin_id(),
        }
    }
}

struct MergeEntry {
    plugin: LoadedPlugin,
    key: MergeSortKey,
}
