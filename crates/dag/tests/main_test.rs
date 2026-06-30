use dag::*;
use std::collections::{HashMap, HashSet};

struct Node {
    id: &'static str,
    depends: &'static [&'static str],
}

impl DagNode for Node {
    fn id(&self) -> &str {
        self.id
    }
    fn depends(&self) -> impl IntoIterator<Item = &impl AsRef<str>> {
        self.depends
    }
}

#[test]
fn unknown_dependency_detection() {
    let nodes = vec![Node {
        id: "A",
        depends: &["B"],
    }];

    let res = nodes.try_dag();
    assert! { matches!(res, Err(DagError::UnknownDependency { .. })) };
}

#[test]
fn duplicate_node_detection() {
    let nodes = vec![
        Node {
            id: "A",
            depends: &[],
        },
        Node {
            id: "A",
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
            id: "A",
            depends: &["B"],
        },
        Node {
            id: "B",
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
            id: "A",
            depends: &["B", "C"],
        },
        Node {
            id: "B",
            depends: &["C"],
        },
        Node {
            id: "C",
            depends: &[],
        },
    ];

    let res = nodes.try_dag().unwrap();

    let mut resolved_nodes = res.into_iter();
    assert_eq!(resolved_nodes.next().unwrap().id, "C");
    assert_eq!(resolved_nodes.next().unwrap().id, "B");
    assert_eq!(resolved_nodes.next().unwrap().id, "A");
}

#[test]
fn get_direct_dependents() {
    let nodes = vec![
        Node {
            id: "A",
            depends: &["B"],
        },
        Node {
            id: "B",
            depends: &["C"],
        },
        Node {
            id: "C",
            depends: &[],
        },
    ];
    let res = nodes.try_dag().unwrap();
    let direct_dependents_map: HashMap<_, _> = res
        .into_map_iter(|mut d| {
            (
                d.inner.id().to_string(),
                d.dependents_iter
                    .next()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| a.id().to_string())
                    .collect::<HashSet<_>>(),
            )
        })
        .collect();

    let dependents = direct_dependents_map.get("A").unwrap();
    assert!(dependents.is_empty());

    let dependents = direct_dependents_map.get("B").unwrap();
    assert_eq!(dependents.len(), 1);
    dependents.contains("A");

    let dependents = direct_dependents_map.get("C").unwrap();
    assert_eq!(dependents.len(), 1);
    dependents.contains("B");
}

#[test]
fn get_recursive_dependents() {
    let nodes = vec![
        Node {
            id: "A",
            depends: &["B"],
        },
        Node {
            id: "B",
            depends: &["C"],
        },
        Node {
            id: "C",
            depends: &[],
        },
    ];
    let res = nodes.try_dag().unwrap();
    let recursive_dependents_map: HashMap<_, _> = res
        .into_map_iter(|d| {
            (
                d.inner.id().to_string(),
                d.dependents_iter
                    .flatten()
                    .map(|a| a.id().to_string())
                    .collect::<HashSet<_>>(),
            )
        })
        .collect();

    let dependents = recursive_dependents_map.get("A").unwrap();
    assert!(dependents.is_empty());

    let dependents = recursive_dependents_map.get("B").unwrap();
    assert_eq!(dependents.len(), 1);
    dependents.contains("A");

    let dependents = recursive_dependents_map.get("C").unwrap();
    assert_eq!(dependents.len(), 2);
    dependents.contains("A");
    dependents.contains("B");
}
