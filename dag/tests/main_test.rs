use dag::*;

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

    let res = nodes.try_dag();
    assert! { res.is_ok() };

    let mut resolved_nodes = res.unwrap().into_iter();
    assert_eq!(resolved_nodes.next().unwrap().id, "C");
    assert_eq!(resolved_nodes.next().unwrap().id, "B");
    assert_eq!(resolved_nodes.next().unwrap().id, "A");
}
