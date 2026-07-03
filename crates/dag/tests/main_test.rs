use dag::*;
use std::collections::{HashMap, HashSet};

struct Node {
    id: Option<&'static str>,
    depends: &'static [&'static str],
}

impl DagNode for Node {
    fn id(&self) -> Option<&str> {
        self.id
    }
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>> {
        self.depends
    }
}

#[test]
fn unknown_dependency_detection() {
    let nodes = vec![Node {
        id: Some("A"),
        depends: &["B"],
    }];

    let res = nodes.try_dag();
    assert! { matches!(res, Err(DagError::UnknownDependency { .. })) };
}

#[test]
fn duplicate_node_detection() {
    let nodes = vec![
        Node {
            id: Some("A"),
            depends: &[],
        },
        Node {
            id: Some("A"),
            depends: &[],
        },
    ];

    let res = nodes.try_dag();
    assert! { matches!(res, Err(DagError::DuplicateName(_))) };
}

#[test]
fn cycle_detection() {
    let nodes = vec![
        Node {
            id: Some("A"),
            depends: &["B"],
        },
        Node {
            id: Some("B"),
            depends: &["A"],
        },
    ];

    let res = nodes.try_dag();
    assert! { matches!(res, Err(DagError::CycleDetected(_))) };
}

#[test]
fn successful_resolution() {
    let nodes = vec![
        Node {
            id: Some("A"),
            depends: &["B", "C"],
        },
        Node {
            id: Some("B"),
            depends: &["C"],
        },
        Node {
            id: Some("C"),
            depends: &[],
        },
    ];

    let res = nodes.try_dag().unwrap();

    let mut resolved_nodes = res.into_iter();
    assert_eq!(resolved_nodes.next().unwrap().id, Some("C"));
    assert_eq!(resolved_nodes.next().unwrap().id, Some("B"));
    assert_eq!(resolved_nodes.next().unwrap().id, Some("A"));
}

#[test]
fn get_direct_dependents() {
    let nodes = vec![
        Node {
            id: Some("A"),
            depends: &["B"],
        },
        Node {
            id: Some("B"),
            depends: &["C"],
        },
        Node {
            id: Some("C"),
            depends: &[],
        },
    ];
    let res = nodes.try_dag().unwrap();
    let direct_dependents_map: HashMap<String, HashSet<String>> = res
        .into_map_iter(|mut d| {
            (
                d.inner.id.map(str::to_string).unwrap_or_default(),
                d.dependents_iter
                    .next()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| a.id.map(str::to_string).unwrap_or_default())
                    .collect::<HashSet<_>>(),
            )
        })
        .collect();

    let dependents = direct_dependents_map.get("A").unwrap();
    assert!(dependents.is_empty());

    let dependents = direct_dependents_map.get("B").unwrap();
    assert_eq!(dependents.len(), 1);
    assert!(dependents.contains("A"));

    let dependents = direct_dependents_map.get("C").unwrap();
    assert_eq!(dependents.len(), 1);
    assert!(dependents.contains("B"));
}

#[test]
fn get_recursive_dependents() {
    let nodes = vec![
        Node {
            id: Some("A"),
            depends: &["B"],
        },
        Node {
            id: Some("B"),
            depends: &["C"],
        },
        Node {
            id: Some("C"),
            depends: &[],
        },
    ];
    let res = nodes.try_dag().unwrap();
    let recursive_dependents_map: HashMap<String, HashSet<String>> = res
        .into_map_iter(|d| {
            (
                d.inner.id.map(str::to_string).unwrap_or_default(),
                d.dependents_iter
                    .flatten()
                    .map(|a| a.id.map(str::to_string).unwrap_or_default())
                    .collect::<HashSet<_>>(),
            )
        })
        .collect();

    let dependents = recursive_dependents_map.get("A").unwrap();
    assert!(dependents.is_empty());

    let dependents = recursive_dependents_map.get("B").unwrap();
    assert_eq!(dependents.len(), 1);
    assert!(dependents.contains("A"));

    let dependents = recursive_dependents_map.get("C").unwrap();
    assert_eq!(dependents.len(), 2);
    assert!(dependents.contains("A"));
    assert!(dependents.contains("B"));
}

#[test]
fn multiple_unnamed_nodes_coexist() {
    // 名前なしノードは複数共存できる（重複チェック対象外）。
    let nodes = vec![
        Node {
            id: None,
            depends: &[],
        },
        Node {
            id: None,
            depends: &[],
        },
    ];
    let res = nodes.try_dag();
    assert!(res.is_ok());
}

#[test]
fn unnamed_node_is_not_dependency_target() {
    // 名前なしノードは被依存にならない。存在しない名前への依存は UnknownDependency。
    let nodes = vec![
        Node {
            id: None,
            depends: &[],
        },
        Node {
            id: Some("A"),
            depends: &["__nonexistent__"],
        },
    ];
    let res = nodes.try_dag();
    assert!(matches!(res, Err(DagError::UnknownDependency { .. })));
}

#[test]
fn unnamed_node_can_depend_on_named() {
    // 名前なしノードは名前ありノードに依存できる。
    let nodes = vec![
        Node {
            id: Some("base"),
            depends: &[],
        },
        Node {
            id: None,
            depends: &["base"],
        },
    ];
    let res = nodes.try_dag().unwrap();
    let ids: Vec<_> = res.into_iter().map(|n| n.id).collect();
    // base（依存される側）が先、名前なし（依存する側）が後。
    assert_eq!(ids, vec![Some("base"), None]);
}

#[test]
fn map_iter_receives_index_and_depth() {
    // index は元の入力順、depth は依存の最深。
    let nodes = vec![
        Node {
            id: Some("leaf"),
            depends: &[],
        }, // index 0, depth 0
        Node {
            id: Some("root"),
            depends: &["leaf"],
        }, // index 1, depth 1
    ];
    let res = nodes.try_dag().unwrap();
    let collected: HashMap<String, (usize, usize)> = res
        .into_map_iter(|d| {
            (
                d.inner.id.map(str::to_string).unwrap_or_default(),
                (d.index, d.depth),
            )
        })
        .collect();
    assert_eq!(collected["leaf"], (0, 0));
    assert_eq!(collected["root"], (1, 1));
}
